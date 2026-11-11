use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::capability::{CapabilityConfig, Stage};
use crate::plugin::{self, SharedPlugin};
use crate::spawn_named;

/// forwarder.yaml 的頂層設定。
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub plugins: PluginsConfig,
    pub stages: PipelineStages,
    pub input: InputConfig,
    pub batch: BatchConfig,
    #[serde(default)]
    pub transport: TransportConfig,
    pub endpoint: EndpointSet,
    #[serde(default = "default_reload_secs")]
    pub config_reload_secs: u64,
    /// 即時狀態的輸出間隔（秒），0 為關閉；可熱更新。
    #[serde(default = "default_status_interval_secs")]
    pub status_interval_secs: u64,
    /// 每個 stage 的 WASI 能力授權（未設定＝全部拒絕，見 capability.rs）。
    #[serde(default)]
    pub capabilities: CapabilityConfig,
}

fn default_reload_secs() -> u64 {
    10
}
fn default_status_interval_secs() -> u64 {
    1
}

/// 各階段的 WASM 路徑；相對路徑以「設定檔所在目錄/plugin/」為基準。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub parse: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub transport: String,
}

/// 啟用哪些階段（parse 一定啟用）。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineStages {
    #[serde(default)]
    pub filter: bool,
    #[serde(default)]
    pub format: bool,
    #[serde(default)]
    pub transport: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputConfig {
    pub mode: InputMode,
    pub tcp: Option<TcpInputConfig>,
    pub tail: Option<TailInputConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    Tcp,
    Tail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TcpInputConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TailInputConfig {
    pub path: String,
}

/// 批次與資源參數。未知欄位會直接報錯，避免舊設定（例如 processing_mode）被默默忽略。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchConfig {
    /// 每個 plugin instance 的線性記憶體上限（MB）。
    pub mem_limit_mb: usize,
    /// 輸出 flush 時間窗 W（ms）；可熱更新。
    pub max_wait_ms: u64,
    /// 輸入 channel 容量（行）。
    pub channel_capacity: usize,
    /// 每次呼叫 format plugin 的最大 entry 數。
    pub max_format_chunk: usize,
    /// 每個 HTTP POST 的最大 byte 數（含 NDJSON 換行）。
    pub max_transport_bytes: usize,
    pub transport_workers: usize,
    /// parse→filter／filter→format／format→transport 三條 channel 的容量（批）。
    #[serde(default = "default_pipeline_channel_capacity")]
    pub pipeline_channel_capacity: usize,
    /// 每次 plugin 呼叫的時限 T（ms）。
    #[serde(default = "default_stage_timeout_ms")]
    pub stage_timeout_ms: u64,
    /// true＝instance 跨批重用；false＝每批用完即丟。
    #[serde(default = "default_plugin_instance_reuse")]
    pub plugin_instance_reuse: bool,
    /// 重用中的 instance 線性記憶體超過 mem_limit_mb 的這個百分比時，換成新的 instance。
    /// 插件沒有釋放記憶體（例如 TinyGo 不回收 host 傳入的參數）時，可在超過上限前換新，插件不必自行處理。
    #[serde(default = "default_instance_recycle_percent")]
    pub instance_recycle_percent: u8,
    /// 僅供實驗（消融）：false 時關閉呼叫時限，卡死的插件會讓該階段永久停住。
    #[serde(default = "default_true")]
    pub watchdog: bool,
    /// 僅供實驗（消融）：false 時不做二分隔離，失敗的整批直接寫入 dead-letter。
    #[serde(default = "default_true")]
    pub bisection: bool,
    /// 處理批次的位元組上限（KiB）；可熱更新。
    #[serde(default = "default_micro_batch_max_bytes_kib")]
    pub micro_batch_max_bytes_kib: usize,
    /// 處理批次的筆數上限；可熱更新。
    #[serde(default = "default_micro_batch_records")]
    pub micro_batch_records: usize,
    /// 處理批次自第一筆到達起最多等待的時間（ms）；可熱更新。
    #[serde(default = "default_micro_batch_max_wait_ms")]
    pub micro_batch_max_wait_ms: u64,
}

fn default_pipeline_channel_capacity() -> usize {
    2
}
fn default_stage_timeout_ms() -> u64 {
    30_000
}
fn default_plugin_instance_reuse() -> bool {
    true
}
fn default_true() -> bool {
    true
}
fn default_instance_recycle_percent() -> u8 {
    50
}
fn default_micro_batch_max_bytes_kib() -> usize {
    128
}
fn default_micro_batch_records() -> usize {
    99_999
}
fn default_micro_batch_max_wait_ms() -> u64 {
    5
}

impl BatchConfig {
    pub fn flush_interval(&self) -> Duration {
        Duration::from_millis(self.max_wait_ms)
    }

    pub fn stage_timeout(&self) -> Duration {
        Duration::from_millis(self.stage_timeout_ms)
    }

    pub fn mem_limit_bytes(&self) -> usize {
        self.mem_limit_mb * 1024 * 1024
    }

    pub fn instance_policy(&self) -> InstancePolicy {
        InstancePolicy {
            reuse: self.plugin_instance_reuse,
            recycle_bytes: self.mem_limit_bytes() / 100 * self.instance_recycle_percent as usize,
            bisection: self.bisection,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !(1..=100).contains(&self.instance_recycle_percent) {
            return Err(format!(
                "instance_recycle_percent={} 需介於 1..100",
                self.instance_recycle_percent
            ));
        }
        if self.mem_limit_mb < 64 {
            return Err(format!(
                "mem_limit_mb={} 過小（建議 >= 64 MB）",
                self.mem_limit_mb
            ));
        }
        if self.max_wait_ms == 0 {
            return Err("max_wait_ms 不能為 0".to_string());
        }
        for (name, value) in [
            ("channel_capacity", self.channel_capacity),
            ("max_format_chunk", self.max_format_chunk),
            ("transport_workers", self.transport_workers),
            ("pipeline_channel_capacity", self.pipeline_channel_capacity),
        ] {
            if value == 0 {
                return Err(format!("{name} 不能為 0"));
            }
        }
        if self.max_transport_bytes < 4096 {
            return Err(format!(
                "max_transport_bytes={} 過小（至少 4096 B）",
                self.max_transport_bytes
            ));
        }
        if self.stage_timeout_ms == 0 {
            return Err("stage_timeout_ms 不能為 0".to_string());
        }
        if self.micro_batch_records == 0 || self.micro_batch_records >= 100_000 {
            return Err(format!(
                "micro_batch_records={} 需介於 1..100000（host id 每批跨距 100,000）",
                self.micro_batch_records
            ));
        }
        if self.micro_batch_max_wait_ms == 0 {
            return Err("micro_batch_max_wait_ms 不能為 0".to_string());
        }
        // 整批會送進單一 WASM store，需保留空間給處理過程的中間配置
        let batch_bytes = self.micro_batch_max_bytes_kib as f64 * 1024.0;
        if self.micro_batch_max_bytes_kib == 0 || batch_bytes > self.mem_limit_bytes() as f64 * 0.7
        {
            return Err(format!(
                "micro_batch_max_bytes_kib={} 需大於 0 且不超過 mem_limit_mb 的 70%",
                self.micro_batch_max_bytes_kib
            ));
        }
        Ok(())
    }
}

/// instance 的重用方式。
#[derive(Debug, Clone, Copy)]
pub struct InstancePolicy {
    /// 跨批重用 instance
    pub reuse: bool,
    /// 重用中的 instance 線性記憶體達到這個大小時換新
    pub recycle_bytes: usize,
    /// 失敗時以二分法找出問題資料；false 時整批拒絕（僅供實驗）
    pub bisection: bool,
}

/// Transport 的送出與重試參數。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u32,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u32,
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: default_connect_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
            retry: RetryConfig::default(),
        }
    }
}

/// 端點暫時無法使用時的重試策略（指數退避加抖動）；用完後資料寫入 error.txt。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            initial_backoff_ms: default_initial_backoff_ms(),
            max_backoff_ms: default_max_backoff_ms(),
        }
    }
}

fn default_connect_timeout_ms() -> u32 {
    5_000
}
fn default_request_timeout_ms() -> u32 {
    30_000
}
fn default_max_retries() -> u32 {
    8
}
fn default_initial_backoff_ms() -> u64 {
    1_000
}
fn default_max_backoff_ms() -> u64 {
    30_000
}

impl TransportConfig {
    fn validate(&self) -> Result<(), String> {
        if self.connect_timeout_ms == 0 || self.request_timeout_ms == 0 {
            return Err("transport 的 connect_timeout_ms／request_timeout_ms 不能為 0".to_string());
        }
        if self.retry.initial_backoff_ms == 0
            || self.retry.max_backoff_ms < self.retry.initial_backoff_ms
        {
            return Err(
                "transport.retry 需滿足 0 < initial_backoff_ms <= max_backoff_ms".to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EndpointSet(pub HashMap<String, String>);

impl AppConfig {
    /// 階段組合與 plugin 路徑檢查。
    fn validate(&self) -> Result<(), String> {
        if !self.stages.format && self.stages.transport {
            return Err(
                "stages.transport=true 需要 stages.format=true（Transport 只接收格式化後的資料）"
                    .to_string(),
            );
        }
        let required = [
            (true, "parse", &self.plugins.parse),
            (self.stages.filter, "filter", &self.plugins.filter),
            (self.stages.format, "format", &self.plugins.format),
            (self.stages.transport, "transport", &self.plugins.transport),
        ];
        for (enabled, name, path) in required {
            if enabled && path.trim().is_empty() {
                return Err(format!(
                    "{name} 階段已啟用，但 plugins.{name} 沒有指定 .wasm"
                ));
            }
        }
        self.capabilities.validate()?;
        self.batch.validate()?;
        self.transport.validate()
    }
}

pub fn load_app_config(path: &str) -> Result<AppConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read config '{path}': {e}"))?;
    let cfg: AppConfig =
        serde_yaml::from_str(&content).map_err(|e| format!("cannot parse config '{path}': {e}"))?;
    cfg.validate()
        .map_err(|e| format!("invalid config '{path}': {e}"))?;
    Ok(cfg)
}

/// 相對路徑以「設定檔所在目錄/plugin/」為基準；啟動與熱更換共用同一套規則。
pub fn resolve_plugin_path(config_path: &str, plugin: &str) -> String {
    let path = Path::new(plugin);
    if path.is_absolute() {
        return plugin.to_string();
    }
    let dir = Path::new(config_path).parent().unwrap_or(Path::new(""));
    dir.join("plugin").join(path).to_string_lossy().to_string()
}

/// 各階段的 plugin slot（未啟用的階段為 None）。
#[derive(Clone)]
pub struct PluginSlots {
    pub parse: SharedPlugin,
    pub filter: Option<SharedPlugin>,
    pub format: Option<SharedPlugin>,
    pub transport: Option<SharedPlugin>,
}

/// 背景重新讀取設定檔：批次參數下一批生效；plugin 路徑或檔案 mtime 變動時熱更換。
pub fn spawn_config_watcher(path: String, shared: Arc<RwLock<AppConfig>>, slots: PluginSlots) {
    struct Watched {
        stage: Stage,
        slot: SharedPlugin,
        path: String,
        mtime: Option<std::time::SystemTime>,
    }

    fn file_mtime(p: &str) -> Option<std::time::SystemTime> {
        std::fs::metadata(p).ok()?.modified().ok()
    }

    fn configured(plugins: &PluginsConfig, stage: Stage) -> &str {
        match stage {
            Stage::Parse => &plugins.parse,
            Stage::Filter => &plugins.filter,
            Stage::Format => &plugins.format,
            Stage::Transport => &plugins.transport,
        }
    }

    spawn_named("cfg-watcher", move || {
        let mut watched: Vec<Watched> = {
            let cfg = shared.read().unwrap();
            [
                (Stage::Parse, Some(slots.parse)),
                (Stage::Filter, slots.filter),
                (Stage::Format, slots.format),
                (Stage::Transport, slots.transport),
            ]
            .into_iter()
            .filter_map(|(stage, slot)| {
                let resolved = resolve_plugin_path(&path, configured(&cfg.plugins, stage));
                let mtime = file_mtime(&resolved);
                Some(Watched {
                    stage,
                    slot: slot?,
                    path: resolved,
                    mtime,
                })
            })
            .collect()
        };

        let mut interval = Duration::from_secs(shared.read().unwrap().config_reload_secs.max(1));
        let mut next_check = Instant::now() + interval;
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if Instant::now() < next_check {
                continue;
            }
            let new_cfg = match load_app_config(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[config] reload rejected（沿用舊設定）: {e}");
                    next_check = Instant::now() + interval;
                    continue;
                }
            };
            interval = Duration::from_secs(new_cfg.config_reload_secs.max(1));
            next_check = Instant::now() + interval;
            for w in &mut watched {
                let resolved = resolve_plugin_path(&path, configured(&new_cfg.plugins, w.stage));
                let mtime = file_mtime(&resolved);
                if resolved != w.path || mtime != w.mtime {
                    let grant = new_cfg.capabilities.grant(w.stage);
                    if plugin::rebuild_shared_slot(&w.slot, &resolved, w.stage, grant) {
                        w.path = resolved;
                        w.mtime = mtime;
                    }
                }
            }
            *shared.write().unwrap() = new_cfg;
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn yaml(extra_batch: &str, stages: &str) -> String {
        format!(
            r#"
plugins: {{ parse: p.wasm, filter: f.wasm, format: o.wasm, transport: t.wasm }}
stages: {stages}
input: {{ mode: tcp, tcp: {{ host: "0.0.0.0", port: 5140 }} }}
batch: {{ mem_limit_mb: 128, max_wait_ms: 100, channel_capacity: 10, max_format_chunk: 10, max_transport_bytes: 4096, transport_workers: 1 {extra_batch} }}
endpoint: {{ A: "http://127.0.0.1:8080/a", B: "http://127.0.0.1:8080/b" }}
"#
        )
    }

    fn check(y: &str) -> Result<(), String> {
        let cfg: AppConfig = serde_yaml::from_str(y).map_err(|e| e.to_string())?;
        cfg.validate()
    }

    const ALL: &str = "{ filter: true, format: true, transport: true }";

    #[test]
    fn streaming_defaults_match_measured_settings() {
        let cfg: AppConfig = serde_yaml::from_str(&yaml("", ALL)).unwrap();
        assert_eq!(cfg.batch.micro_batch_max_bytes_kib, 128);
        assert_eq!(cfg.batch.micro_batch_max_wait_ms, 5);
        assert_eq!(cfg.batch.pipeline_channel_capacity, 2);
        assert!(cfg.batch.plugin_instance_reuse);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn removed_batch_mode_is_an_error_not_silently_ignored() {
        let err = check(&yaml(", processing_mode: batch", ALL)).unwrap_err();
        assert!(err.contains("processing_mode"), "{err}");
    }

    #[test]
    fn transport_without_format_is_rejected() {
        let err = check(&yaml(
            "",
            "{ filter: true, format: false, transport: true }",
        ))
        .unwrap_err();
        assert!(err.contains("stages.format"), "{err}");
    }

    #[test]
    fn enabled_stage_needs_a_plugin_path() {
        let y = yaml("", ALL).replace("filter: f.wasm, ", "");
        let err = check(&y).unwrap_err();
        assert!(err.contains("plugins.filter"), "{err}");
    }

    #[test]
    fn zero_stage_timeout_is_rejected() {
        assert!(
            check(&yaml(", stage_timeout_ms: 0", ALL))
                .unwrap_err()
                .contains("stage_timeout_ms")
        );
    }

    #[test]
    fn startup_and_hot_swap_resolve_the_same_path() {
        assert_eq!(
            resolve_plugin_path("/etc/w/forwarder.yaml", "p.wasm"),
            "/etc/w/plugin/p.wasm"
        );
        assert_eq!(
            resolve_plugin_path("forwarder.yaml", "p.wasm"),
            "plugin/p.wasm"
        );
        assert_eq!(
            resolve_plugin_path("/etc/w/forwarder.yaml", "/opt/p.wasm"),
            "/opt/p.wasm"
        );
    }
}
