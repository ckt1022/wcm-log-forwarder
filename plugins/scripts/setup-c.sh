#!/usr/bin/env bash
# 下載並安裝編譯 C 插件需要的工具。可重複執行：已安裝且版本正確的項目會略過。
source "$(dirname "$0")/lib.sh"

# 1. wasi-sdk（clang、wasi-libc，以及直接產生 component 的 wasm-component-ld）
install_wasi_sdk

# 2. wit-bindgen CLI（從 WIT 產生 C 綁定），以 cargo 安裝固定版本
if ! "$TOOLCHAIN/bin/wit-bindgen" --version 2>/dev/null | grep -q " $WIT_BINDGEN_VERSION$"; then
    cargo install --locked --root "$TOOLCHAIN" wit-bindgen-cli --version "$WIT_BINDGEN_VERSION"
fi
"$TOOLCHAIN/bin/wit-bindgen" --version

# 3. cJSON（Parse 使用的 JSON 函式庫）
CJSON_DIR="$TOOLCHAIN/cjson-$CJSON_VERSION"
mkdir -p "$CJSON_DIR"
fetch "$CJSON_URL/cJSON.c" "$CJSON_DIR/cJSON.c" sha256 "$CJSON_C_SHA256"
fetch "$CJSON_URL/cJSON.h" "$CJSON_DIR/cJSON.h" sha256 "$CJSON_H_SHA256"
echo "cJSON $CJSON_VERSION"

echo "C 工具鏈已就緒：$TOOLCHAIN"
