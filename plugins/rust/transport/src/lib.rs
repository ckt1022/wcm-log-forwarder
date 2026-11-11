// lib.rs — wasi:http transport-plugin
//
// 邏輯對應 c-plugin/transport_http/transport_http_impl.c：
// 使用 wasi:http/outgoing-handler，host (hyper) 負責 TCP 連線 / TLS / HTTP
// framing；插件只需描述 OutgoingRequest 並呼叫 handle()。
//
// 支援 http:// 與 https://（TLS 由 host 決定，插件本身不做任何 TLS 處理）。

wit_bindgen::generate!({
    path: "../../../core/wit",
    world: "transport-plugin",
    generate_all,
});

use std::sync::Mutex;

use crate::local::log_process::transport_types::{AuthMethod, RetryConfig};
use crate::wasi::clocks::monotonic_clock;
use crate::wasi::http::outgoing_handler;
use crate::wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};

// ───────────────────────────────────────────────────────────
// 插件狀態
// ───────────────────────────────────────────────────────────

struct State {
    https: bool,
    authority: String, // "host" 或 "host:port"（預設 port 省略）
    path: String,
    auth: AuthMethod,
    retry: Option<RetryConfig>,
    max_batch_bytes: u32,
    extra_headers: Vec<(String, String)>,
    bytes_sent: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

// ───────────────────────────────────────────────────────────
// 小工具：錯誤 / base64 / sleep
// ───────────────────────────────────────────────────────────

fn config_err(msg: impl Into<String>) -> PluginError {
    PluginError::ConfigError(msg.into())
}

fn processing_err(msg: impl Into<String>) -> PluginError {
    PluginError::ProcessingFailed(msg.into())
}

fn sleep_ms(ms: u64) {
    monotonic_clock::subscribe_duration(ms * 1_000_000).block();
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ───────────────────────────────────────────────────────────
// endpoint 解析：http://host[:port][/path] 或 https://...
// 回傳 (https, authority, path)
// ───────────────────────────────────────────────────────────

fn parse_endpoint(endpoint: &str) -> Result<(bool, String, String), PluginError> {
    let (rest, https, default_port) = if let Some(r) = endpoint.strip_prefix("http://") {
        (r, false, 80u16)
    } else if let Some(r) = endpoint.strip_prefix("https://") {
        (r, true, 443u16)
    } else {
        return Err(config_err(
            "unsupported scheme (only http:// and https:// are supported)",
        ));
    };

    let (authority_raw, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    if authority_raw.is_empty() {
        return Err(config_err("endpoint is missing a host"));
    }

    let (host, port) = match authority_raw.rfind(':') {
        Some(idx) => {
            let port = authority_raw[idx + 1..]
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .unwrap_or(default_port);
            (&authority_raw[..idx], port)
        }
        None => (authority_raw, default_port),
    };

    if host.is_empty() {
        return Err(config_err("endpoint host must not be empty"));
    }

    // authority for the Host/authority field: omit default port
    let is_default = (!https && port == 80) || (https && port == 443);
    let authority = if is_default {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };

    Ok((https, authority, path.to_string()))
}

// ───────────────────────────────────────────────────────────
// 送出單一 HTTP 請求（wasi:http outgoing-handler）
// ───────────────────────────────────────────────────────────

fn send_http_request(body: &[u8], state: &State) -> Result<(), PluginError> {
    // ── 1. 建立請求 headers ──────────────────────────────────────────────────
    let headers = Fields::new();
    headers
        .append("content-type", b"application/octet-stream")
        .map_err(|e| processing_err(format!("failed to set content-type header: {e:?}")))?;

    match &state.auth {
        AuthMethod::None => {}
        AuthMethod::BearerToken(token) => {
            headers
                .append("authorization", format!("Bearer {token}").as_bytes())
                .map_err(|e| processing_err(format!("failed to set auth header: {e:?}")))?;
        }
        AuthMethod::BasicAuth(cred) => {
            let encoded =
                base64_encode(format!("{}:{}", cred.username, cred.password).as_bytes());
            headers
                .append("authorization", format!("Basic {encoded}").as_bytes())
                .map_err(|e| processing_err(format!("failed to set auth header: {e:?}")))?;
        }
        AuthMethod::ApiKey(cfg) => {
            headers
                .append(&cfg.header_name, cfg.key.as_bytes())
                .map_err(|e| processing_err(format!("failed to set auth header: {e:?}")))?;
        }
    }

    for (name, value) in &state.extra_headers {
        headers
            .append(name, value.as_bytes())
            .map_err(|e| processing_err(format!("failed to set extra header: {e:?}")))?;
    }

    // ── 2. 建立 OutgoingRequest（消耗 headers）──────────────────────────────
    let request = OutgoingRequest::new(headers);
    request
        .set_method(&Method::Post)
        .map_err(|_| processing_err("failed to set method"))?;
    let scheme = if state.https { Scheme::Https } else { Scheme::Http };
    request
        .set_scheme(Some(&scheme))
        .map_err(|_| processing_err("failed to set scheme"))?;
    request
        .set_authority(Some(&state.authority))
        .map_err(|_| processing_err("failed to set authority"))?;
    request
        .set_path_with_query(Some(&state.path))
        .map_err(|_| processing_err("failed to set path"))?;

    // ── 3. 取得 body 的 output stream ────────────────────────────────────────
    let out_body = request
        .body()
        .map_err(|_| processing_err("failed to get outgoing body handle"))?;

    // ── 4. 先送出請求（消耗 request）────────────────────────────────────────
    // 必須在寫 body 之前 handle()：handle() 之前寫入的 body 只能暫存在
    // host 的 4096B 緩衝，超過即 deadlock（blocking_write_and_flush 永遠
    // 等不到緩衝釋放）；先 handle() 讓 host 開始消耗 stream，
    // body 大小就不再受限（對應 max-batch-bytes > 4096 的設定）。
    let future = outgoing_handler::handle(request, None)
        .map_err(|e| processing_err(format!("outgoing-handler.handle() failed: {e:?}")))?;

    {
        let stream = out_body
            .write()
            .map_err(|_| processing_err("failed to get outgoing body stream"))?;

        // ── 5. 分批寫入 body（blocking_write_and_flush 每次最多 4096 B）──────
        for chunk in body.chunks(4096) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|e| processing_err(format!("body write failed: {e:?}")))?;
        }
        // stream 在此 scope 結束時 drop，必須發生在 finish 之前
    }

    // ── 6. Finish body（消耗 out_body，無 trailers）──────────────────────────
    OutgoingBody::finish(out_body, None)
        .map_err(|e| processing_err(format!("failed to finish outgoing body: {e:?}")))?;

    // ── 7. 等待回應（poll）──────────────────────────────────────────────────
    future.subscribe().block();

    // ── 8. 取得回應並檢查 status code ───────────────────────────────────────
    let response = future
        .get()
        .ok_or_else(|| processing_err("future not ready after polling"))?
        .map_err(|_| processing_err("HTTP future returned no value after poll"))?
        .map_err(|e| processing_err(format!("HTTP request failed: {e:?}")))?;

    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(processing_err(format!("server returned HTTP {status}")));
    }

    Ok(())
}

// ───────────────────────────────────────────────────────────
// 帶重試的送出（指數退避）
// ───────────────────────────────────────────────────────────

fn send_with_retry(body: &[u8], state: &State) -> Result<(), PluginError> {
    let (max_retries, mut backoff_ms, max_backoff_ms) = match &state.retry {
        Some(r) => (r.max_retries, r.initial_backoff_ms as u64, r.max_backoff_ms as u64),
        None => (0, 100, 5000),
    };

    let mut attempt = 0u32;
    loop {
        match send_http_request(body, state) {
            Ok(()) => return Ok(()),
            Err(_) if attempt < max_retries => {
                attempt += 1;
                sleep_ms(backoff_ms);
                backoff_ms = (backoff_ms * 2).min(max_backoff_ms);
            }
            Err(e) => return Err(e),
        }
    }
}

// ───────────────────────────────────────────────────────────
// batch 切割（依 max-batch-bytes，0 = 不限制）
// 每筆 entry 若結尾沒有換行則補上 '\n'
// ───────────────────────────────────────────────────────────

fn build_batches(output_data: &[Vec<u8>], max_batch_bytes: u32) -> Vec<Vec<u8>> {
    let mut batches = Vec::new();
    let limit = max_batch_bytes as usize;
    let mut cur: Vec<u8> = Vec::new();

    for entry in output_data {
        if limit > 0 && !cur.is_empty() && cur.len() + entry.len() + 1 > limit {
            batches.push(std::mem::take(&mut cur));
        }
        cur.extend_from_slice(entry);
        if entry.last() != Some(&b'\n') {
            cur.push(b'\n');
        }
    }
    if !cur.is_empty() {
        batches.push(cur);
    }

    batches
}

// ───────────────────────────────────────────────────────────
// Exports
// ───────────────────────────────────────────────────────────

struct Component;

impl Guest for Component {
    fn init(config: TransportConfig) -> Result<(), PluginError> {
        // 重複呼叫視為重新設定（不累加舊設定）
        let mut guard = STATE.lock().unwrap();
        *guard = None;

        if config.endpoint.is_empty() {
            return Err(config_err("endpoint must not be empty"));
        }

        let (https, authority, path) = parse_endpoint(&config.endpoint)?;

        *guard = Some(State {
            https,
            authority,
            path,
            auth: config.auth,
            retry: config.retry,
            max_batch_bytes: config.max_batch_bytes,
            extra_headers: config.extra_headers,
            bytes_sent: 0,
        });
        Ok(())
    }

    fn transport(output_data: Vec<Vec<u8>>) -> Result<(), PluginError> {
        let mut guard = STATE.lock().unwrap();
        let state = guard
            .as_mut()
            .ok_or_else(|| config_err("init() not called before transport()"))?;

        if output_data.is_empty() {
            return Ok(());
        }

        let batches = build_batches(&output_data, state.max_batch_bytes);

        let mut total_sent = 0u64;
        for batch in &batches {
            send_with_retry(batch, state)?;
            total_sent += batch.len() as u64;
        }

        state.bytes_sent += total_sent;
        Ok(())
    }

    fn report_usage() -> u64 {
        STATE
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |s| s.bytes_sent)
    }
}

export!(Component);
