# demo 用的 WCM 映像：由 core/ 的原始碼建置，內含 core/plugin 的插件（一般與故障測試插件）。
# 建置（在 demo/ 目錄）：./setup.sh，或 docker build -f Dockerfile -t wcm-demo:latest ../core
FROM rust:1.93.1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wcm-base-log-forwarder /usr/local/bin/wcm-base-log-forwarder
# 設定檔中的相對插件路徑以 /app/plugin/ 為基準
COPY plugin /app/plugin
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/wcm-base-log-forwarder"]
CMD ["/app/forwarder.yaml"]
