#!/usr/bin/env bash
# 下載並安裝編譯 Go 插件需要的工具與套件。可重複執行：已安裝且版本正確的項目會略過。
source "$(dirname "$0")/lib.sh"
source scripts/go-env.sh

# 1. Go
if ! "$TOOLCHAIN/go/bin/go" version 2>/dev/null | grep -q "go$GO_VERSION "; then
    tarball="$DOWNLOADS/$(basename "$GO_URL")"
    fetch "$GO_URL" "$tarball" sha256 "$GO_SHA256"
    tar -xzf "$tarball" -C "$TOOLCHAIN"
fi
go version

# 2. TinyGo（把 Go 編譯成 WebAssembly component）
if ! "$TOOLCHAIN/tinygo/bin/tinygo" version 2>/dev/null | grep -q "version $TINYGO_VERSION "; then
    tarball="$DOWNLOADS/$(basename "$TINYGO_URL")"
    fetch "$TINYGO_URL" "$tarball" sha256 "$TINYGO_SHA256"
    tar -xzf "$tarball" -C "$TOOLCHAIN"
fi
tinygo version

# 3. wasm-tools（TinyGo 從 PATH 呼叫它把 core module 包成 component），以 cargo 安裝固定版本
if ! "$TOOLCHAIN/bin/wasm-tools" --version 2>/dev/null | grep -q "^wasm-tools $WASM_TOOLS_VERSION"; then
    cargo install --locked --root "$TOOLCHAIN" wasm-tools --version "$WASM_TOOLS_VERSION"
fi
wasm-tools --version

# 4. wit-bindgen-go（從 WIT 產生 Go 綁定）；go install 會以 sum.golang.org 校驗模組
if ! "$TOOLCHAIN/bin/wit-bindgen-go" version 2>/dev/null | grep -q "$WIT_BINDGEN_GO_VERSION"; then
    GOBIN="$TOOLCHAIN/bin" go install "go.bytecodealliance.org/cmd/wit-bindgen-go@$WIT_BINDGEN_GO_VERSION"
fi
wit-bindgen-go version

# 5. 插件依賴的 Go 模組（版本與雜湊值由 go/go.mod、go/go.sum 鎖定）
(cd go && go mod download)
echo "Go 工具鏈已就緒：$TOOLCHAIN"
