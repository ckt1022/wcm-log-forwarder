# 讓 dotnet 使用 .toolchain/ 內的 SDK 與套件快取，不讀寫使用者目錄下的設定。
# 用法：source scripts/dotnet-env.sh（需在 plugins/ 目錄執行）
export DOTNET_ROOT="$PWD/.toolchain/dotnet"
export PATH="$DOTNET_ROOT:$PATH"
export NUGET_PACKAGES="$PWD/.toolchain/nuget"
export DOTNET_CLI_HOME="$PWD/.toolchain/dotnet-home"
export DOTNET_NOLOGO=1
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1
