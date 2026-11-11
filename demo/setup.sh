#!/usr/bin/env bash
# 準備 demo 需要的映像：由 ../core 建置 WCM（wcm-demo:latest），並下載 Python 映像（接收端、送出端、結果整理）。
# 只需要 Docker；第一次建置需要網路，約數分鐘。修改 core/ 後重新執行。
set -euo pipefail
DEMO_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$DEMO_DIR/lib/images.env"
echo "=== 建置 $WCM_IMAGE（由 $(cd "$DEMO_DIR/../core" && pwd)）"
docker build -f "$DEMO_DIR/Dockerfile" -t "$WCM_IMAGE" "$DEMO_DIR/../core"
echo "=== 下載 $PY_IMAGE"
docker pull -q "$PY_IMAGE"
echo "完成"
