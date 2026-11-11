#!/usr/bin/env bash
# 下載並安裝編譯 C# 插件需要的工具與 NuGet 套件。可重複執行：已安裝的項目會略過。
source "$(dirname "$0")/lib.sh"
source scripts/dotnet-env.sh

# 1. .NET SDK（版本同時由 csharp/global.json 鎖定）
if ! "$DOTNET_ROOT/dotnet" --list-sdks 2>/dev/null | grep -q "^$DOTNET_SDK_VERSION "; then
    tarball="$DOWNLOADS/$(basename "$DOTNET_SDK_URL")"
    fetch "$DOTNET_SDK_URL" "$tarball" sha512 "$DOTNET_SDK_SHA512"
    mkdir -p "$DOTNET_ROOT"
    tar -xzf "$tarball" -C "$DOTNET_ROOT"
fi
echo ".NET SDK $("$DOTNET_ROOT/dotnet" --version)"

# 2. wasi-sdk（NativeAOT-LLVM 連結 WebAssembly 時使用）
install_wasi_sdk

# 3. NuGet 套件：依各專案的 packages.lock.json 還原到 .toolchain/nuget（第一次約需 10 分鐘）
for stage in parse filter format transport; do
    dotnet restore "csharp/$stage" --locked-mode
done
echo "C# 工具鏈已就緒：$TOOLCHAIN"
