// Transport 階段：router 依 targettag 把資料分流到各端點，worker 呼叫 plugin 送出。
//
// 失敗處理參考現有日誌轉發器的做法：
//   - 可重試（連線錯誤、逾時、HTTP 408／429／5xx，501 除外）：與 Vector 的預設相同，
//     以指數退避加抖動重試（Fluent Bit 的公式：等待時間在 base 與 min(base·2^n, cap) 之間隨機）。
//     重試期間 worker 不取新工作，壓力自然回傳到上游。
//   - 不可重試（其他 4xx、設定錯誤）：重送同一批沒有意義，交給二分隔離找出造成錯誤的資料。
//   - trap：與其他階段相同，換新 instance 重試一次，再二分。
//   - 重試用完：與 Fluent Bit 的 dead letter queue、Fluentd 的 secondary 相同，資料寫入檔案，不丟棄。
//
// 語意為 at-least-once（與 Vector 相同）：逾時或 trap 時無法得知端點是否已收到，重送可能造成重複。

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use wasmtime::Store;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::sockets::SocketAddrUse;

use super::FormattedBatch;
use super::runner::{StageBackend, StageRunner};
use crate::capability::SocketGrant;
use crate::config::{AppConfig, RetryConfig};
use crate::isolation::{IsolationLimits, ProbeOutcome};
use crate::metrics::StageStats;
use crate::plugin::local::log_process::pipeline_process::PluginError;
use crate::plugin::transport_bindings::TransportPlugin;
use crate::plugin::transport_bindings::local::log_process::transport_types::{
    AuthMethod, TransportConfig,
};
use crate::plugin::{self, HostState, HttpGuard, SharedPlugin};
use crate::{dead_letter, shutdown, spawn_named};

/// router→worker 佇列長度。
const WORK_QUEUE_TASKS: usize = 1024;

/// 永久錯誤時的二分呼叫上限：每次探測都是一個網路請求。
const MAX_REJECTED_PROBES: u32 = 64;

/// 等待退避時檢查 shutdown 的間隔。
const BACKOFF_POLL: Duration = Duration::from_millis(100);

/// router 分發給 worker 的傳送任務。
struct SendTask {
    url: String,
    lines: Vec<Vec<u8>>,
}

fn line_record(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

// ── Router ───────────────────────────────────────────────────────────────────

type EndpointBuffers = HashMap<String, (Vec<Vec<u8>>, usize)>;

fn flush_buffer(
    key: &str,
    buf: &mut Vec<Vec<u8>>,
    endpoints: &HashMap<String, String>,
    tx: &SyncSender<SendTask>,
    stats: &StageStats,
) {
    if buf.is_empty() {
        return;
    }
    let lines = std::mem::take(buf);
    let Some(url) = endpoints.get(key).cloned() else {
        stats.unroutable_records.add(lines.len() as u64);
        eprintln!(
            "[router] unknown endpoint key '{key}', writing {} lines to dead-letter",
            lines.len()
        );
        dead_letter::write(
            &format!("[router] 未知的端點 '{key}'"),
            lines.iter().map(|l| line_record(l)),
        );
        return;
    };
    if let Err(e) = tx.send(SendTask { url, lines }) {
        stats.unroutable_records.add(e.0.lines.len() as u64);
        dead_letter::write(
            &format!("[router] worker 已關閉 url={}", e.0.url),
            e.0.lines.iter().map(|l| line_record(l)),
        );
    }
}

fn flush_all(
    buffers: &mut EndpointBuffers,
    endpoints: &HashMap<String, String>,
    tx: &SyncSender<SendTask>,
    stats: &StageStats,
) {
    for (key, (buf, bytes)) in buffers.iter_mut() {
        flush_buffer(key, buf, endpoints, tx, stats);
        *bytes = 0;
    }
}

/// 依 targettag 分流；targettag 的每個字母是一個端點（"AB" 會送到 A 和 B）。
fn route_batch(
    batch: FormattedBatch,
    buffers: &mut EndpointBuffers,
    endpoints: &HashMap<String, String>,
    tx: &SyncSender<SendTask>,
    max_bytes: usize,
    stats: &StageStats,
) {
    for (line, tag) in batch.lines.into_iter().zip(batch.targettags.iter()) {
        if tag.is_empty() {
            stats.unroutable_records.add(1);
            dead_letter::write("[router] 沒有 targettag", [line_record(&line)]);
            continue;
        }
        for key in tag.chars().map(|c| c.to_string()) {
            // 以含 '\n' 的 NDJSON body 計算；「加入會超過上限」時先送出
            let framed_len = line.len() + 1;
            let (buf, buf_bytes) = buffers.entry(key.clone()).or_default();
            if !buf.is_empty() && *buf_bytes + framed_len > max_bytes {
                flush_buffer(&key, buf, endpoints, tx, stats);
                *buf_bytes = 0;
            }
            *buf_bytes += framed_len;
            buf.push(line.clone());
        }
    }
}

/// 處理完的日誌一到就進端點 buffer，送出條件為：
///   (a) 全域週期計時器每 W = max_wait_ms 觸發一次（與 Fluent Bit 的 [SERVICE] Flush 相同），
///       日誌在 buffer 的停留時間介於 [0, W)；
///   (b) 單一端點 buffer 再加一行會超過 max_transport_bytes。
/// W 每個 tick 重新讀取設定，可熱更新。
fn router(
    rx: Receiver<FormattedBatch>,
    tx: SyncSender<SendTask>,
    cfg: Arc<RwLock<AppConfig>>,
    max_bytes: usize,
    stats: Arc<StageStats>,
) {
    let read_interval = || {
        cfg.read()
            .unwrap()
            .batch
            .flush_interval()
            .max(Duration::from_millis(1))
    };
    let endpoints = || cfg.read().unwrap().endpoint.0.clone();
    let mut buffers = EndpointBuffers::new();
    let mut interval = read_interval();
    let mut next_tick = Instant::now() + interval;

    loop {
        let now = Instant::now();
        if now >= next_tick {
            flush_all(&mut buffers, &endpoints(), &tx, &stats);
            interval = read_interval();
            while next_tick <= now {
                next_tick += interval;
            }
            continue;
        }
        match rx.recv_timeout(next_tick - now) {
            Ok(batch) => route_batch(batch, &mut buffers, &endpoints(), &tx, max_bytes, &stats),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                flush_all(&mut buffers, &endpoints(), &tx, &stats);
                break;
            }
        }
    }
}

// ── 端點白名單 ────────────────────────────────────────────────────────────────

/// 從端點 URL 解析出 (host, port)。
fn parse_endpoint_host_port(url: &str) -> Option<(String, u16)> {
    let (default_port, rest) = if let Some(r) = url.strip_prefix("https://") {
        (443u16, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (80u16, r)
    } else {
        return None;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    if let Some(idx) = authority.rfind(':') {
        let port: u16 = authority[idx + 1..].parse().ok()?;
        Some((authority[..idx].to_string(), port))
    } else {
        Some((authority.to_string(), default_port))
    }
}

/// raw socket 白名單：只允許 TCP connect 到 endpoint map 中的 host:port。每次 connect 即時比對，支援熱更新。
fn build_endpoint_socket_check(
    cfg: Arc<RwLock<AppConfig>>,
) -> impl Fn(SocketAddr, SocketAddrUse) -> Pin<Box<dyn Future<Output = bool> + Send + Sync>>
+ Send
+ Sync
+ 'static {
    move |addr: SocketAddr, use_: SocketAddrUse| {
        let cfg = Arc::clone(&cfg);
        Box::pin(async move {
            if !matches!(use_, SocketAddrUse::TcpConnect) {
                return false;
            }
            let endpoints = cfg.read().unwrap().endpoint.0.clone();
            for url in endpoints.values() {
                let Some((host, port)) = parse_endpoint_host_port(url) else {
                    continue;
                };
                if port != addr.port() {
                    continue;
                }
                if let Ok(ip) = host.parse::<IpAddr>() {
                    if ip == addr.ip() {
                        return true;
                    }
                } else if let Ok(mut resolved) =
                    tokio::net::lookup_host((host.as_str(), port)).await
                {
                    if resolved.any(|a| a.ip() == addr.ip()) {
                        return true;
                    }
                }
            }
            eprintln!("[SECURITY][transport] 拒絕連線：{addr} 不在 endpoint map 中");
            false
        })
    }
}

/// wasi:http 白名單：只放行 endpoint map 中的 host:port。每次請求即時比對，支援熱更新。
fn build_endpoint_http_guard(cfg: Arc<RwLock<AppConfig>>) -> HttpGuard {
    Arc::new(move |host: &str, port: u16| {
        let c = cfg.read().unwrap();
        c.endpoint.0.values().any(|url| {
            parse_endpoint_host_port(url)
                .is_some_and(|(h, p)| p == port && h.eq_ignore_ascii_case(host))
        })
    })
}

// ── 重試策略 ──────────────────────────────────────────────────────────────────

/// 從 plugin 錯誤訊息取出 HTTP status（WIT 沒有結構化欄位，C／Rust 插件都以 "HTTP <code>" 回報）。
fn http_status(message: &str) -> Option<u16> {
    let rest = message.split("HTTP ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

/// 端點暫時無法使用（值得重試）或資料本身的問題（不值得）。
/// 沒有 HTTP status 的錯誤視為連線問題，寧可重試也不丟資料。
fn is_retriable(error: &PluginError) -> bool {
    match error {
        PluginError::InvalidInput(_) | PluginError::ConfigError(_) | PluginError::UnknownId(_) => {
            false
        }
        PluginError::InternalError => true,
        PluginError::ProcessingFailed(message) => match http_status(message) {
            Some(status) => {
                status == 408 || status == 429 || ((500..600).contains(&status) && status != 501)
            }
            None => true,
        },
    }
}

/// 第 attempt 次重試（從 0 起）的等待時間上限。
fn backoff_ceiling(policy: &RetryConfig, attempt: u32) -> u64 {
    let doubled = policy
        .initial_backoff_ms
        .saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX));
    doubled.min(policy.max_backoff_ms)
}

/// 在 [initial, ceiling] 之間隨機，避免多個 worker 同時重試。
fn backoff_delay(policy: &RetryConfig, attempt: u32, random: u64) -> Duration {
    let floor = policy.initial_backoff_ms;
    let ceiling = backoff_ceiling(policy, attempt).max(floor);
    Duration::from_millis(floor + random % (ceiling - floor + 1))
}

/// 等待退避時間；收到 shutdown 時回傳 false，資料立刻交給 dead-letter。
fn wait_backoff(delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if shutdown::requested() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep(BACKOFF_POLL.min(deadline - now));
    }
}

// ── Worker ───────────────────────────────────────────────────────────────────

struct TransportBackend {
    shared: SharedPlugin,
    mem_limit_bytes: usize,
    cfg: Arc<RwLock<AppConfig>>,
    runtime: tokio::runtime::Runtime,
    url: String,
    max_batch_bytes: u32,
    connect_timeout_ms: u32,
    request_timeout_ms: u32,
    retry: RetryConfig,
    stats: Arc<StageStats>,
    random_state: u64,
}

struct TransportInstance {
    store: Store<HostState>,
    plugin: TransportPlugin,
    initialized_url: Option<String>,
}

impl TransportBackend {
    /// 網路等待的總時限（host await 期間 epoch 攔不到）。
    fn wall_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms as u64 + self.request_timeout_ms as u64)
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64
        let mut x = self.random_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.random_state = x;
        x
    }

    fn plugin_config(&self) -> TransportConfig {
        TransportConfig {
            endpoint: self.url.clone(),
            auth: AuthMethod::None,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            // 重試由 host 統一負責，避免與 plugin 內的重試疊加
            retry: None,
            tls: None,
            extra_headers: vec![],
            max_batch_bytes: self.max_batch_bytes,
        }
    }

    /// instance 尚未對目前的端點 init 時先 init。
    fn ensure_initialized(
        &mut self,
        inst: &mut TransportInstance,
        ticks: u64,
    ) -> Result<(), ProbeOutcome<()>> {
        if inst.initialized_url.as_deref() == Some(self.url.as_str()) {
            return Ok(());
        }
        let config = self.plugin_config();
        plugin::arm_deadline(&mut inst.store, ticks);
        match self
            .runtime
            .block_on(inst.plugin.call_init(&mut inst.store, &config))
        {
            Ok(Ok(())) => {
                inst.initialized_url = Some(self.url.clone());
                Ok(())
            }
            Ok(Err(e)) => Err(ProbeOutcome::GaveUp(format!(
                "init failed url={}: {e:?}",
                self.url
            ))),
            Err(e) => Err(ProbeOutcome::Trapped(format!(
                "init trap url={}: {e}",
                self.url
            ))),
        }
    }
}

impl StageBackend for TransportBackend {
    type Item = Vec<u8>;
    type Output = ();
    type Instance = TransportInstance;

    fn label(&self) -> &'static str {
        "transport"
    }

    fn plugin_version(&self) -> u64 {
        self.shared.read().unwrap().version
    }

    fn instantiate(&mut self) -> wasmtime::Result<TransportInstance> {
        let (component, linker) = {
            let slot = self.shared.read().unwrap();
            (slot.component.clone(), slot.linker.clone())
        };
        let (socket_grant, guard) = {
            let socket_grant = self.cfg.read().unwrap().capabilities.transport.sockets;
            (
                socket_grant,
                build_endpoint_http_guard(Arc::clone(&self.cfg)),
            )
        };
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdout().inherit_stderr();
        // raw sockets 只有明確授權 endpoint 時才開 DNS 並套用白名單；其餘情況維持全拒
        if socket_grant == SocketGrant::Endpoint {
            builder
                .allow_ip_name_lookup(true)
                .socket_addr_check(build_endpoint_socket_check(Arc::clone(&self.cfg)));
        }
        let mut store = plugin::new_store(self.mem_limit_bytes, builder.build(), Some(guard));
        let plugin = self.runtime.block_on(TransportPlugin::instantiate_async(
            &mut store, &component, &linker,
        ))?;
        Ok(TransportInstance {
            store,
            plugin,
            initialized_url: None,
        })
    }

    /// 送出一批。網路請求的時間由 wall_timeout 限制，二分時也不縮短，避免把慢的端點誤判成 poison。
    fn invoke(
        &mut self,
        inst: &mut TransportInstance,
        items: &[Vec<u8>],
        _ticks: u64,
    ) -> ProbeOutcome<()> {
        let wall_timeout = self.wall_timeout();
        let ticks = plugin::ticks_for(wall_timeout + Duration::from_secs(1));
        let mut retries_used = 0;
        loop {
            if let Err(outcome) = self.ensure_initialized(inst, ticks) {
                return outcome;
            }
            plugin::arm_deadline(&mut inst.store, ticks);
            let result = self.runtime.block_on(async {
                tokio::time::timeout(
                    wall_timeout,
                    inst.plugin.call_transport(&mut inst.store, items),
                )
                .await
            });
            let message = match result {
                Ok(Ok(Ok(()))) => return ProbeOutcome::Accepted(Vec::new()),
                Ok(Err(trap)) => return ProbeOutcome::Trapped(trap.to_string()),
                Ok(Ok(Err(error))) => {
                    let message = format!("{error:?}");
                    if !is_retriable(&error) {
                        return ProbeOutcome::Rejected(message);
                    }
                    message
                }
                Err(_) => {
                    // 呼叫中途被放棄，這個 instance 不能再用
                    *inst = match self.instantiate() {
                        Ok(fresh) => fresh,
                        Err(e) => {
                            return ProbeOutcome::Trapped(format!(
                                "rebuild after timeout failed: {e}"
                            ));
                        }
                    };
                    format!("request timed out after {wall_timeout:?}")
                }
            };

            if retries_used >= self.retry.max_retries || shutdown::requested() {
                return ProbeOutcome::GaveUp(format!(
                    "url={} after {retries_used} retries: {message}",
                    self.url
                ));
            }
            let random = self.next_random();
            let delay = backoff_delay(&self.retry, retries_used, random);
            eprintln!(
                "[transport] send failed url={} retry {}/{} in {delay:?}: {message}",
                self.url,
                retries_used + 1,
                self.retry.max_retries
            );
            if !wait_backoff(delay) {
                return ProbeOutcome::GaveUp(format!(
                    "url={} shutdown during retry: {message}",
                    self.url
                ));
            }
            retries_used += 1;
            self.stats.delivery_retries.add(1);
        }
    }

    fn memory_peak(&self, inst: &TransportInstance) -> usize {
        inst.store.data().limiter.wasm_mem_peak
    }

    fn limits(&self) -> IsolationLimits {
        IsolationLimits {
            max_rejected_calls: MAX_REJECTED_PROBES,
            ..crate::isolation::DEFAULT_LIMITS
        }
    }
}

fn worker(
    id: usize,
    rx: Arc<Mutex<Receiver<SendTask>>>,
    shared: SharedPlugin,
    cfg: Arc<RwLock<AppConfig>>,
    stats: Arc<StageStats>,
) -> wasmtime::Result<()> {
    let (ticks, reuse, mem_limit_bytes, max_bytes, connect_timeout_ms, request_timeout_ms, retry) = {
        let c = cfg.read().unwrap();
        (
            plugin::ticks_for(c.batch.stage_timeout()),
            c.batch.instance_policy(),
            c.batch.mem_limit_bytes(),
            c.batch.max_transport_bytes,
            c.transport.connect_timeout_ms,
            c.transport.request_timeout_ms,
            c.transport.retry.clone(),
        )
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ ((id as u64 + 1) << 32);
    let backend = TransportBackend {
        shared,
        mem_limit_bytes,
        cfg,
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
        url: String::new(),
        max_batch_bytes: max_bytes as u32,
        connect_timeout_ms,
        request_timeout_ms,
        retry,
        stats: Arc::clone(&stats),
        random_state: seed | 1,
    };
    let mut runner = StageRunner::new(backend, ticks, reuse, stats)?;
    let mut sequence: u64 = 0;

    loop {
        let task = match rx.lock().unwrap().recv() {
            Ok(t) => t,
            Err(_) => break,
        };
        sequence += 1;
        runner.backend.url = task.url;
        let report = runner.process(sequence, &task.lines);
        dead_letter::write_rejected(
            &format!("transport url={}", runner.backend.url),
            sequence,
            task.lines.len(),
            &report.rejected,
            |i| line_record(&task.lines[i]),
        );
        let delivered: usize = report.accepted.iter().map(|(range, _)| range.len()).sum();
        runner.stats.output_records.add(delivered as u64);
        let delivered_bytes: usize = report
            .accepted
            .iter()
            .flat_map(|(range, _)| task.lines[range.clone()].iter())
            .map(|line| line.len() + 1)
            .sum();
        runner.stats.input_bytes.add(delivered_bytes as u64);
    }
    Ok(())
}

pub fn run(
    rx: Receiver<FormattedBatch>,
    shared: SharedPlugin,
    cfg: Arc<RwLock<AppConfig>>,
    stats: Arc<StageStats>,
) -> wasmtime::Result<()> {
    let (workers, max_bytes) = {
        let c = cfg.read().unwrap();
        (c.batch.transport_workers, c.batch.max_transport_bytes)
    };
    let (tx_work, rx_work) = sync_channel::<SendTask>(WORK_QUEUE_TASKS);
    let rx_work = Arc::new(Mutex::new(rx_work));

    let router_cfg = Arc::clone(&cfg);
    let router_stats = Arc::clone(&stats);
    let router_handle = spawn_named("tx-router", move || {
        router(rx, tx_work, router_cfg, max_bytes, router_stats)
    });

    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let (rx, shared, cfg, stats) = (
                Arc::clone(&rx_work),
                Arc::clone(&shared),
                Arc::clone(&cfg),
                Arc::clone(&stats),
            );
            spawn_named(&format!("tx-worker-{i}"), move || {
                worker(i, rx, shared, cfg, stats)
            })
        })
        .collect();

    // 只讓 worker 持有 receiver：worker 全部提早結束時，router 的 send 會失敗並改寫 dead-letter
    drop(rx_work);
    let _ = router_handle.join();
    for handle in handles {
        handle
            .join()
            .map_err(|_| wasmtime::Error::msg("transport worker panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::yaml;
    use std::thread;

    const ALL: &str = "{ filter: true, format: true, transport: true }";

    fn router_cfg(max_wait_ms: u64) -> Arc<RwLock<AppConfig>> {
        let y = yaml("", ALL).replace("max_wait_ms: 100", &format!("max_wait_ms: {max_wait_ms}"));
        Arc::new(RwLock::new(serde_yaml::from_str(&y).unwrap()))
    }

    fn fbatch(n: usize, line_len: usize, tag: &str, seq: u64) -> FormattedBatch {
        let lines = (0..n)
            .map(|i| {
                let mut l = format!("{seq}-{i}-").into_bytes();
                l.resize(line_len.max(l.len()), b'x');
                l
            })
            .collect();
        FormattedBatch {
            lines,
            targettags: vec![tag.to_string(); n],
        }
    }

    #[test]
    fn router_flushes_on_periodic_timer() {
        let (tx_f, rx_f) = sync_channel::<FormattedBatch>(16);
        let (tx_w, rx_w) = sync_channel::<SendTask>(16);
        let cfg = router_cfg(100);
        let h = thread::spawn(move || router(rx_f, tx_w, cfg, 4096, Arc::default()));
        let t0 = Instant::now();
        tx_f.send(fbatch(3, 20, "A", 0)).unwrap();
        tx_f.send(fbatch(2, 20, "AB", 1)).unwrap();
        assert!(
            rx_w.recv_timeout(Duration::from_millis(30)).is_err(),
            "不應在 tick 前送出"
        );
        let mut tasks = vec![rx_w.recv_timeout(Duration::from_secs(2)).unwrap()];
        tasks.extend(rx_w.recv_timeout(Duration::from_millis(50)));
        assert!(t0.elapsed() < Duration::from_millis(400));
        tasks.sort_by(|a, b| a.url.cmp(&b.url));
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].url, "http://127.0.0.1:8080/a");
        let a: Vec<String> = tasks[0]
            .lines
            .iter()
            .map(|l| String::from_utf8_lossy(&l[..4]).to_string())
            .collect();
        assert_eq!(a, vec!["0-0-", "0-1-", "0-2-", "1-0-", "1-1-"]);
        assert_eq!(tasks[1].lines.len(), 2);
        assert!(
            rx_w.recv_timeout(Duration::from_millis(250)).is_err(),
            "閒置期間不送空 POST"
        );
        drop(tx_f);
        h.join().unwrap();
    }

    #[test]
    fn router_flushes_on_size_before_timer_and_on_close() {
        let (tx_f, rx_f) = sync_channel::<FormattedBatch>(16);
        let (tx_w, rx_w) = sync_channel::<SendTask>(16);
        let cfg = router_cfg(60_000);
        let h = thread::spawn(move || router(rx_f, tx_w, cfg, 4096, Arc::default()));
        tx_f.send(fbatch(10, 1000, "A", 0)).unwrap();
        let t1 = rx_w.recv_timeout(Duration::from_secs(1)).unwrap();
        let t2 = rx_w.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!((t1.lines.len(), t2.lines.len()), (4, 4));
        assert!(rx_w.recv_timeout(Duration::from_millis(100)).is_err());
        drop(tx_f);
        assert_eq!(
            rx_w.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .lines
                .len(),
            2
        );
        h.join().unwrap();
    }

    #[test]
    fn http_guard_only_allows_endpoint_map() {
        let guard = build_endpoint_http_guard(router_cfg(100));
        assert!(guard("127.0.0.1", 8080));
        assert!(!guard("127.0.0.1", 9090));
        assert!(!guard("169.254.169.254", 80));
    }

    fn failed(message: &str) -> PluginError {
        PluginError::ProcessingFailed(message.to_string())
    }

    #[test]
    fn retriable_classification_matches_vector_defaults() {
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                is_retriable(&failed(&format!("server returned HTTP {code}"))),
                "{code}"
            );
        }
        for code in [400, 401, 403, 404, 413, 501] {
            assert!(
                !is_retriable(&failed(&format!("server returned HTTP {code}"))),
                "{code}"
            );
        }
        assert!(
            is_retriable(&failed("connection refused")),
            "沒有 status 視為連線問題"
        );
        assert!(is_retriable(&failed(
            "HTTP request failed (error-code tag 3)"
        )));
        assert!(is_retriable(&PluginError::InternalError));
        assert!(!is_retriable(&PluginError::ConfigError("x".into())));
        assert!(!is_retriable(&PluginError::InvalidInput("x".into())));
    }

    #[test]
    fn backoff_stays_between_initial_and_doubling_ceiling() {
        let policy = RetryConfig {
            max_retries: 8,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
        };
        assert_eq!(backoff_ceiling(&policy, 0), 1000);
        assert_eq!(backoff_ceiling(&policy, 3), 8000);
        assert_eq!(backoff_ceiling(&policy, 10), 30_000);
        assert_eq!(
            backoff_ceiling(&policy, 200),
            30_000,
            "位移溢位時仍受上限保護"
        );
        for random in [0, 1, 7, 12_345, u64::MAX] {
            let d = backoff_delay(&policy, 3, random);
            assert!(
                d >= Duration::from_millis(1000) && d <= Duration::from_millis(8000),
                "{d:?}"
            );
        }
    }

    #[test]
    fn endpoint_host_port_parsing() {
        assert_eq!(
            parse_endpoint_host_port("http://h:81/x"),
            Some(("h".into(), 81))
        );
        assert_eq!(
            parse_endpoint_host_port("https://h/x"),
            Some(("h".into(), 443))
        );
        assert_eq!(parse_endpoint_host_port("ftp://h"), None);
    }
}
