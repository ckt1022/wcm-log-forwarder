#!/usr/bin/env bash
# 準備五產品實驗需要的一切：工具、產品映像、WCM 映像、Fluent Bit WASM filter、分析用的 Python 環境。
# 可重複執行。需要 Docker 權限與網路。
set -euo pipefail
COMMON="$(cd "$(dirname "$0")" && pwd)"
EXPERIMENT="$(dirname "$COMMON")"
REPO="$(dirname "$EXPERIMENT")"
source "$COMMON/products.env"

step() { echo "=== $*"; }

# 1. 負載產生器與 HTTP 接收端（以固定版本的 Go 映像編譯，產生靜態執行檔）
step "建置 gen 與 httpsink（$GO_BUILD_IMAGE）"
mkdir -p "$COMMON/bin"
for tool in gen httpsink; do
    docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -e CGO_ENABLED=0 -e GOFLAGS=-trimpath \
        -v "$COMMON/tools/$tool:/src:ro" -v "$COMMON/bin:/out" -w /src \
        "$GO_BUILD_IMAGE" go build -o "/out/$tool" main.go
done
ls -la "$COMMON/bin"

# 2. 第三方產品映像
step "下載產品映像"
for image in "$FLUENTBIT_IMAGE" "$FLUENTD_IMAGE" "$VECTOR_IMAGE"; do
    docker pull -q "$image"
done

# 3. WCM 映像（由 core/ 原始碼建置）
step "建置 WCM 映像 $WCM_IMAGE"
docker build -q -f "$COMMON/wcm/Dockerfile" -t "$WCM_IMAGE" "$REPO/core"

# 4. Fluent Bit WASM filter：每個實驗的 products/fluentbit-wasm/filter/ 各自編譯
step "建置 Fluent Bit WASM filter（$RUST_BUILD_IMAGE，wasm32-wasip1）"
for filter in "$EXPERIMENT"/five_product/*/products/fluentbit-wasm/filter; do
    [[ -f "$filter/Cargo.toml" ]] || continue
    docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
        -v "$filter:/src" -w /src "$RUST_BUILD_IMAGE" \
        sh -c "rustup target add wasm32-wasip1 >/dev/null 2>&1; cargo build --release --locked --target wasm32-wasip1"
    cp "$filter"/target/wasm32-wasip1/release/*.wasm "$(dirname "$filter")/filter.wasm"
    echo "built $(dirname "$filter")/filter.wasm"
done

# 5. 記錄實際使用的映像 digest
step "記錄映像版本到 images.lock"
{
    echo "# 由 setup.sh 產生：實驗實際使用的映像（$(date -Is)）"
    for image in "$FLUENTBIT_IMAGE" "$FLUENTD_IMAGE" "$VECTOR_IMAGE" "$WCM_IMAGE" "$GO_BUILD_IMAGE" "$RUST_BUILD_IMAGE"; do
        echo "$image $(docker image inspect -f '{{.Id}}' "$image" 2>/dev/null) $(docker image inspect -f '{{join .RepoDigests " "}}' "$image" 2>/dev/null)"
    done
} > "$COMMON/images.lock"
cat "$COMMON/images.lock"

# 6. 分析與畫圖用的 Python 環境
step "建立 Python 環境"
python3 -m venv "$COMMON/venv"
"$COMMON/venv/bin/pip" install -q -r "$COMMON/requirements.txt"
echo "完成"
