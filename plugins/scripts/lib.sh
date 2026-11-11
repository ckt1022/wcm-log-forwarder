# setup 腳本共用的函式。工具一律安裝到 plugins/.toolchain/，不影響系統環境。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
source toolchain.env
TOOLCHAIN="$PWD/.toolchain"
DOWNLOADS="$TOOLCHAIN/downloads"
mkdir -p "$DOWNLOADS"

# fetch <網址> <目的檔> <演算法 sha256|sha512> <校驗值>
# 已存在且校驗正確時不重新下載；校驗失敗時刪除檔案並停止。
fetch() {
    local url=$1 file=$2 algorithm=$3 sum=$4
    if [[ ! -f "$file" ]]; then
        echo "下載 $url"
        curl -fL --retry 5 --retry-delay 5 --retry-all-errors --connect-timeout 30 --progress-bar -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    if ! echo "$sum  $file" | "${algorithm}sum" -c --quiet; then
        echo "校驗失敗：$file"
        rm -f "$file"
        exit 1
    fi
}

# wasi-sdk：解壓到 .toolchain/wasi-sdk-<版本>-x86_64-linux
install_wasi_sdk() {
    local dir="$TOOLCHAIN/wasi-sdk-$WASI_SDK_VERSION-x86_64-linux"
    if [[ ! -x "$dir/bin/clang" ]]; then
        local tarball="$DOWNLOADS/$(basename "$WASI_SDK_URL")"
        fetch "$WASI_SDK_URL" "$tarball" sha256 "$WASI_SDK_SHA256"
        tar -xzf "$tarball" -C "$TOOLCHAIN"
    fi
    echo "wasi-sdk $(head -1 "$dir/VERSION")：$("$dir/bin/clang" --version | head -1)"
}
