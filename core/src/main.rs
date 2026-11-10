mod capability;     // 每個stage的WASI能力限制，管理與檢測插件的能力，進行阻擋。
mod config;         // 讀取.yaml檔案的設定、預設設定。
mod dead_letter;    // 錯誤日誌的處理
mod input;          // 讀取log的邏輯(TCP、讀檔案)
mod isolation;      // 二分隔離錯誤的邏輯
mod metrics;        // 處理的指標
mod output;         // 統計資料
mod pipeline;       // [核心] 四個階段的程式碼(初始化、建立與重試、元件之間的溝通)
mod plugin;         // 每個插件的位置，請擺放編譯好的.wasm component
mod shutdown;       // 關閉系統的處理(channel 依序關閉，各階段處理完佇列內的資料後結束，router 送出剩餘 buffer)

use std::sync::{Arc, RwLock};
use std::thread;

use capability::{FsGrant, SocketGrant, Stage, StageGrant};
use config::{
    AppConfig, InputMode, PluginSlots, load_app_config, resolve_plugin_path, spawn_config_watcher,
};
use plugin::SharedPlugin;

/// 以名稱啟動執行緒（可在 /proc/<pid>/task/*/comm 看到，最多 15 字元）。
pub fn spawn_named<F, T>(name: &str, f: F) -> thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .expect("thread spawn failed")
}

/// `--check-plugin <stage> <wasm> [inert-fs] [inert-sockets|endpoint-sockets]`
/// 只做載入時能力檢查，不啟動 pipeline。
fn check_plugin_cli(args: &[String]) -> ! {
    let usage = "usage: wcm-base-log-forwarder --check-plugin <parse|filter|format|transport> <plugin.wasm> [inert-fs] [inert-sockets|endpoint-sockets]";
    let (Some(stage), Some(path)) = (args.first().and_then(|s| Stage::from_label(s)), args.get(1))
    else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let mut grant = StageGrant::default();
    for a in &args[2..] {
        match a.as_str() {
            "inert-fs" => grant.filesystem = FsGrant::Inert,
            "inert-sockets" => grant.sockets = SocketGrant::Inert,
            "endpoint-sockets" => grant.sockets = SocketGrant::Endpoint,
            other => {
                eprintln!("unknown grant '{other}'\n{usage}");
                std::process::exit(2);
            }
        }
    }
    // 能力檢查通過後再做 instantiate_pre：確認受限 linker 真的能滿足所有 import
    let result = plugin::build_runtime(path, stage, grant)
        .and_then(|(component, linker)| linker.instantiate_pre(&component).map(|_| ()));
    match result {
        Ok(()) => {
            println!("LOAD_OK stage={stage} grant={grant:?} plugin={path}");
            std::process::exit(0);
        }
        Err(e) => {
            println!("LOAD_REFUSED stage={stage} grant={grant:?} plugin={path}\n{e}");
            std::process::exit(1);
        }
    }
}

fn load_slots(config_path: &str, cfg: &AppConfig) -> wasmtime::Result<PluginSlots> {
    let load = |path: &str, stage: Stage| -> wasmtime::Result<SharedPlugin> {
        plugin::new_shared_runtime(
            &resolve_plugin_path(config_path, path),
            stage,
            cfg.capabilities.grant(stage),
        )
    };
    let optional =
        |enabled: bool, path: &str, stage: Stage| -> wasmtime::Result<Option<SharedPlugin>> {
            enabled.then(|| load(path, stage)).transpose()
        };
    Ok(PluginSlots {
        parse: load(&cfg.plugins.parse, Stage::Parse)?,
        filter: optional(cfg.stages.filter, &cfg.plugins.filter, Stage::Filter)?,
        format: optional(cfg.stages.format, &cfg.plugins.format, Stage::Format)?,
        transport: optional(
            cfg.stages.transport,
            &cfg.plugins.transport,
            Stage::Transport,
        )?,
    })
}


fn main() -> wasmtime::Result<()> {
    // 讀取forwarder.yaml檔案
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--check-plugin") {
        check_plugin_cli(&argv[2..]);
    }

    let config_path = argv
        .get(1)
        .cloned()
        .unwrap_or_else(|| "forwarder.yaml".to_string());
    let cfg = match load_app_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[config] {e}");
            std::process::exit(1);
        }
    };
    output::print_startup(&cfg);

    // slot代表每個插槽，初始化每個階段、實例
    let slots = match load_slots(&config_path, &cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return Err(e);
        }
    };

    shutdown::install_handlers();

    // 傳送的指標
    let metrics = Arc::new(metrics::Metrics::default());
    // channel緩衝剛送進來的日誌
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(cfg.batch.channel_capacity);
    match &cfg.input.mode {
        // TCP讀取log
        InputMode::Tcp => {
            let tcp = cfg.input.tcp.as_ref().unwrap_or_else(|| {
                eprintln!("[config] input.tcp section required when mode=tcp");
                std::process::exit(1);
            });
            input::spawn_tcp_reader(tcp.host.clone(), tcp.port, tx, Arc::clone(&metrics));
        }
        // 檔案讀取log
        InputMode::Tail => {
            let tail = cfg.input.tail.as_ref().unwrap_or_else(|| {
                eprintln!("[config] input.tail section required when mode=tail");
                std::process::exit(1);
            });
            input::spawn_tail_reader(tail.path.clone(), tx, Arc::clone(&metrics));
        }
    }

    // watch config，在forwarder.yaml檔案有更動時，準備做熱更換插件
    let shared_cfg = Arc::new(RwLock::new(cfg));
    spawn_config_watcher(config_path, Arc::clone(&shared_cfg), slots.clone());

    // 啟動pipeline
    pipeline::run_pipeline(rx, slots, shared_cfg, metrics)
}
