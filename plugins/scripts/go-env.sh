# 讓 go、tinygo、wasm-tools 使用 .toolchain/ 內的版本與快取，不讀寫使用者目錄下的設定。
# 用法：source scripts/go-env.sh（需在 plugins/ 目錄執行）
export PATH="$PWD/.toolchain/bin:$PWD/.toolchain/tinygo/bin:$PWD/.toolchain/go/bin:$PATH"
export GOROOT="$PWD/.toolchain/go"
export GOPATH="$PWD/.toolchain/gopath"
export GOMODCACHE="$GOPATH/pkg/mod"
export GOCACHE="$PWD/.toolchain/gocache"
# 不自動下載其他 Go 版本；模組版本以 go.sum 為準
export GOTOOLCHAIN=local
export GOFLAGS=-mod=readonly
