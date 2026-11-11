# WCM 的實驗映像：由 core/ 的原始碼建置，內含 core/plugin 的 Rust 插件與故障測試插件。
# 建置（在 repo 根目錄）：docker build -f experiment/common/wcm/Dockerfile -t wcm-log-forwarder:experiment core
FROM rust:1.93.1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wcm-base-log-forwarder /usr/local/bin/wcm-base-log-forwarder
# 相對的插件路徑以設定檔所在目錄/plugin/ 為基準：設定檔掛在 /app/forwarder.yaml
COPY plugin /app/plugin
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/wcm-base-log-forwarder"]
CMD ["/app/forwarder.yaml"]
