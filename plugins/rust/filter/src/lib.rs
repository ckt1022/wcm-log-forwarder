// lib.rs — level-based reduction-plugin
//
// 邏輯對應 c-plugin/filter/filter_impl.c 的 LevelFilter：
// 保留 level >= Warn 的 log（丟棄 debug / info），純常數版本、不讀 runtime config。

wit_bindgen::generate!({
    path: "../../../core/wit",
    world: "reduction-plugin",
    generate_all,
});

use std::sync::atomic::{AtomicU64, Ordering};

use crate::local::log_process::pipeline_process::LogLevel;
use wasi::clocks::monotonic_clock;

const MIN_KEEP_LEVEL: LogLevel = LogLevel::Warn;

static LAST_EXEC_NS: AtomicU64 = AtomicU64::new(0);

struct Component;

impl Guest for Component {
    fn filter(struct_data: Vec<LogEntry>) -> Result<Vec<FilterResult>, PluginError> {
        let start_ns = monotonic_clock::now();

        let results = struct_data
            .iter()
            .map(|entry| FilterResult {
                id: entry.id,
                keep: entry.level as u8 >= MIN_KEEP_LEVEL as u8,
            })
            .collect();

        LAST_EXEC_NS.store(
            monotonic_clock::now().saturating_sub(start_ns),
            Ordering::Relaxed,
        );

        Ok(results)
    }

    fn report_usage() -> u64 {
        LAST_EXEC_NS.load(Ordering::Relaxed)
    }
}

export!(Component);
