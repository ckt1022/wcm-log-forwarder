// Wasmtime 相關的共用部分：WIT 綁定、記憶體限制、host state、plugin 的載入與熱更換。

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use wasmtime::{
    Config, Engine, OptLevel, ResourceLimiter, Store, Strategy,
    component::{Component, Linker},
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::types::{
    HostFutureIncomingResponse, OutgoingRequestConfig, default_send_request,
};

use crate::capability::{self, Stage, StageGrant};
use crate::spawn_named;

// parser-plugin world：產生 ParserPlugin 及 pipeline-process 所有型別
wasmtime::component::bindgen!({
    world: "parser-plugin",
    path: "wit",
});

// 其他 world 用 `with` 重用上面的型別，channel 傳遞時無需轉換。
pub mod format_bindings {
    wasmtime::component::bindgen!({
        world: "format-plugin",
        path: "wit",
        with: {
            "local:log-process/pipeline-process":
                super::local::log_process::pipeline_process,
        }
    });
}

pub mod filter_bindings {
    wasmtime::component::bindgen!({
        world: "reduction-plugin",
        path: "wit",
        with: {
            "local:log-process/pipeline-process":
                super::local::log_process::pipeline_process,
        }
    });
}

// transport-plugin world：async 模式（需 WASI HTTP）。
pub mod transport_bindings {
    wasmtime::component::bindgen!({
        world: "transport-plugin",
        path: "wit",
        exports: { default: async },
        with: {
            "local:log-process/pipeline-process":
                super::local::log_process::pipeline_process,
        }
    });
}

/// 一個 table 允許的元素上限；每個元素約 16 B，上限時約 16 MB。
const MAX_TABLE_ELEMENTS: usize = 1_000_000;

/// WASM 記憶體與 table 的成長限制。
pub struct PluginLimiter {
    mem_limit_bytes: usize,
    pub wasm_mem_peak: usize,
}

impl PluginLimiter {
    pub fn new(mem_limit_bytes: usize) -> Self {
        Self {
            mem_limit_bytes,
            wasm_mem_peak: 0,
        }
    }
}

impl ResourceLimiter for PluginLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.wasm_mem_peak = self.wasm_mem_peak.max(desired);
        Ok(desired <= self.mem_limit_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }
}

/// wasi:http 出站請求的白名單檢查：(host, port) → 是否放行。
pub type HttpGuard = Arc<dyn Fn(&str, u16) -> bool + Send + Sync>;

pub struct HostState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
    pub limiter: PluginLimiter,
    pub http: WasiHttpCtx,
    /// wasi:http 出站白名單；None 代表此 store 不能送 HTTP（parse/filter/format）。
    pub http_guard: Option<HttpGuard>,
}

/// parse／filter／format 共用的 WasiCtx：只繼承 stdout／stderr，
/// 不繼承 stdin、不注入環境變數、不給 preopen、socket 位址檢查維持預設全拒。
pub fn plugin_wasi_ctx() -> WasiCtx {
    WasiCtxBuilder::new()
        .inherit_stdout()
        .inherit_stderr()
        .build()
}

impl WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for HostState {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// wasi:http 不經過 WasiCtx 的 socket_addr_check，因此在送出前比對 host:port，
    /// 不在 endpoint map 中就回傳 HTTP-request-denied。
    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::HttpResult<HostFutureIncomingResponse> {
        let uri = request.uri();
        let host = uri.host().unwrap_or("").to_string();
        let default_port = if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        };
        let port = uri.port_u16().unwrap_or(default_port);
        let allowed = self
            .http_guard
            .as_ref()
            .is_some_and(|guard| guard(&host, port));
        if !allowed {
            eprintln!(
                "[SECURITY][transport] 拒絕 wasi:http 請求：{host}:{port} 不在 endpoint map 中"
            );
            return Err(ErrorCode::HttpRequestDenied.into());
        }
        Ok(default_send_request(request, config))
    }
}

// ── Engine 與 epoch ──────────────────────────────────────────────────────────

/// epoch 的計時單位。
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

/// WASM 實例化（_initialize）期間使用的 epoch deadline（約 28 小時）。
const INSTANTIATE_EPOCH_DEADLINE: u64 = 10_000_000;

/// 整個 process 共用一個 Engine：熱更換只替換 component 與 linker，
/// 舊 instance 也持續收到 epoch tick，時限保護不會中斷。
pub fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.strategy(Strategy::Cranelift);
        config.cranelift_opt_level(OptLevel::Speed);
        config.epoch_interruption(true);
        Engine::new(&config).expect("wasmtime engine")
    })
}

/// 啟動共用的 epoch 計時執行緒（冪等）。
pub fn spawn_epoch_ticker() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        spawn_named("epoch-ticker", || {
            loop {
                std::thread::sleep(EPOCH_TICK);
                engine().increment_epoch();
            }
        });
    });
}

pub fn ticks_for(timeout: Duration) -> u64 {
    (timeout.as_millis() / EPOCH_TICK.as_millis()) as u64
}

/// 建立一個已設定記憶體限制與實例化 deadline 的 Store。
pub fn new_store(
    mem_limit_bytes: usize,
    ctx: WasiCtx,
    http_guard: Option<HttpGuard>,
) -> Store<HostState> {
    let state = HostState {
        ctx,
        table: ResourceTable::new(),
        limiter: PluginLimiter::new(mem_limit_bytes),
        http: WasiHttpCtx::new(),
        http_guard,
    };
    let mut store = Store::new(engine(), state);
    store.limiter(|s| &mut s.limiter);
    store.set_epoch_deadline(INSTANTIATE_EPOCH_DEADLINE);
    store
}

// ── Plugin 載入與熱更換 ───────────────────────────────────────────────────────

pub struct PluginRuntime {
    pub component: Component,
    pub linker: Linker<HostState>,
    /// 每次熱更換遞增；各 stage 在處理批次前比對。
    pub version: u64,
}

pub type SharedPlugin = Arc<RwLock<PluginRuntime>>;

/// 編譯 plugin，依 `stage` 與 `grant` 檢查 import（未授權直接回傳 `[capability]` 錯誤），
/// 並建立只含授權介面的 linker。
pub fn build_runtime(
    wasm_path: &str,
    stage: Stage,
    grant: StageGrant,
) -> wasmtime::Result<(Component, Linker<HostState>)> {
    let engine = engine();
    let component = Component::from_file(engine, wasm_path)?;
    capability::check_component(stage, grant, wasm_path, engine, &component)?;

    let mut linker: Linker<HostState> = Linker::new(engine);
    if stage == Stage::Transport {
        capability::add_wasi_to_linker_async(&mut linker, grant)?;
        wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;
    } else {
        capability::add_wasi_to_linker_sync(&mut linker, grant)?;
    }
    Ok((component, linker))
}

pub fn new_shared_runtime(
    wasm_path: &str,
    stage: Stage,
    grant: StageGrant,
) -> wasmtime::Result<SharedPlugin> {
    let (component, linker) = build_runtime(wasm_path, stage, grant)?;
    Ok(Arc::new(RwLock::new(PluginRuntime {
        component,
        linker,
        version: 0,
    })))
}

/// 重新編譯 plugin 並換入既有 slot。失敗時保留舊版，回傳 false。
pub fn rebuild_shared_slot(
    slot: &SharedPlugin,
    new_path: &str,
    stage: Stage,
    grant: StageGrant,
) -> bool {
    match build_runtime(new_path, stage, grant) {
        Ok((component, linker)) => {
            let mut s = slot.write().unwrap();
            s.component = component;
            s.linker = linker;
            s.version += 1;
            eprintln!(
                "[config] {stage} plugin hot-swapped → v{} ({new_path})",
                s.version
            );
            true
        }
        Err(e) => {
            eprintln!("[config] {stage} plugin rebuild FAILED — keeping old plugin: {e}");
            false
        }
    }
}
