// 四階段管線：input → parse → filter → format → transport。
// 每個階段的 plugin 呼叫都經過 runner.rs：相同的 instance 管理、重試與二分隔離。

mod filter;
mod format;
mod parse;
mod runner;
mod transport;

use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, RwLock};

use crate::config::{AppConfig, PluginSlots};
use crate::metrics::{self, Metrics};
use crate::plugin::local::log_process::pipeline_process::LogEntry;
use crate::{output, plugin, spawn_named};

/// parse／filter 階段的輸出：LogEntry 與各筆對應的 targettag。
pub struct ParsedBatch {
    pub entries: Vec<LogEntry>,
    pub targettags: Vec<String>,
    pub seq: u64,
}

/// format 階段的輸出：格式化後的行與各行對應的 targettag。
pub struct FormattedBatch {
    pub lines: Vec<Vec<u8>>,
    pub targettags: Vec<String>,
}

/// LogEntry 寫入 dead-letter 檔案時的文字形式。
fn entry_record(e: &LogEntry) -> String {
    let tags: Vec<String> = e.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(
        "id={} ts={} level={:?} msg={} tags=[{}]",
        e.id,
        e.timestamp,
        e.level,
        e.message,
        tags.join(",")
    )
}

/// 下游 channel 已關閉：把這批資料保存下來。
fn dead_letter_parsed(stage: &str, batch: ParsedBatch) {
    crate::dead_letter::write(
        &format!("[{stage}] 下游已關閉 batch={}", batch.seq),
        batch.entries.iter().map(entry_record),
    );
}

/// 建立各階段執行緒與 channel，等待全部結束後印出統計。
pub fn run_pipeline(
    rx_raw: Receiver<String>,
    slots: PluginSlots,
    cfg: Arc<RwLock<AppConfig>>,
    metrics: Arc<Metrics>,
) -> wasmtime::Result<()> {
    plugin::spawn_epoch_ticker();
    metrics::spawn_status_reporter(Arc::clone(&cfg), Arc::clone(&metrics));
    let capacity = cfg.read().unwrap().batch.pipeline_channel_capacity;

    let (tx_parsed, rx_parsed) = sync_channel::<ParsedBatch>(capacity);
    let parse_cfg = Arc::clone(&cfg);
    let parse_slot = slots.parse;
    let parse_stats = Arc::clone(&metrics.parse);
    let parse_handle = spawn_named("parse-disp", move || {
        parse::run(rx_raw, tx_parsed, parse_slot, parse_cfg, parse_stats)
    });

    let (rx_for_format, filter_handle) = match slots.filter {
        Some(slot) => {
            let (tx_filtered, rx_filtered) = sync_channel::<ParsedBatch>(capacity);
            let filter_cfg = Arc::clone(&cfg);
            let stats = Arc::clone(&metrics.filter);
            let handle = spawn_named("filter", move || {
                filter::run(rx_parsed, tx_filtered, slot, filter_cfg, stats)
            });
            (rx_filtered, Some(handle))
        }
        None => (rx_parsed, None),
    };

    let (tx_formatted, rx_formatted) = sync_channel::<FormattedBatch>(capacity);
    let format_handle = match slots.format {
        Some(slot) => {
            let format_cfg = Arc::clone(&cfg);
            let stats = Arc::clone(&metrics.format);
            Some(spawn_named("format", move || {
                format::run(rx_for_format, tx_formatted, slot, format_cfg, stats)
            }))
        }
        None => {
            drop(tx_formatted);
            spawn_named(
                "format-drain",
                move || while rx_for_format.recv().is_ok() {},
            );
            None
        }
    };

    let transport_handle = match slots.transport {
        Some(slot) => {
            let transport_cfg = Arc::clone(&cfg);
            let stats = Arc::clone(&metrics.transport);
            Some(spawn_named("transport", move || {
                transport::run(rx_formatted, slot, transport_cfg, stats)
            }))
        }
        None => {
            spawn_named(
                "transport-drain",
                move || while rx_formatted.recv().is_ok() {},
            );
            None
        }
    };

    let results = [
        ("parse", Some(parse_handle), &metrics.parse),
        ("filter", filter_handle, &metrics.filter),
        ("format", format_handle, &metrics.format),
        ("transport", transport_handle, &metrics.transport),
    ];
    let mut first_error = None;
    for (stage, handle, stats) in results {
        let Some(handle) = handle else { continue };
        match handle
            .join()
            .unwrap_or_else(|_| Err(wasmtime::Error::msg(format!("{stage} thread panicked"))))
        {
            Ok(()) => output::print_stage_summary(stage, stats),
            Err(e) => {
                eprintln!("[{stage}] failed: {e}");
                first_error.get_or_insert(e);
            }
        }
    }
    output::print_dead_letter_total();
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
