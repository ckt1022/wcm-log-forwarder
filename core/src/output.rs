// 啟動資訊與結束時的統計。

use crate::config::{AppConfig, PipelineStages};
use crate::dead_letter;
use crate::metrics::StageStats;

pub fn print_startup(cfg: &AppConfig) {
    let PipelineStages {
        filter,
        format,
        transport,
    } = cfg.stages;
    eprintln!("=== WCM Log Forwarder ===");
    eprintln!(
        "Stages       : parse{}{}{}",
        if filter { " → filter" } else { "" },
        if format { " → format" } else { "" },
        if transport { " → transport" } else { "" },
    );
    eprintln!("Memory Limit : {} MB per instance", cfg.batch.mem_limit_mb);
    eprintln!(
        "Micro-batch  : {} KiB / {} records / {} ms",
        cfg.batch.micro_batch_max_bytes_kib,
        cfg.batch.micro_batch_records,
        cfg.batch.micro_batch_max_wait_ms
    );
    eprintln!("Flush Window : {} ms", cfg.batch.max_wait_ms);
    eprintln!("Stage Timeout: {} ms", cfg.batch.stage_timeout_ms);
    eprintln!("Instance Reuse: {}", cfg.batch.plugin_instance_reuse);
    if cfg.status_interval_secs > 0 {
        eprintln!("Status       : every {} s", cfg.status_interval_secs);
    }
    if !transport {
        eprintln!("[config] WARNING stages.transport=false：處理完的資料不會送出（僅供測試）");
    }
    eprintln!("========================");
}

pub fn print_stage_summary(stage: &str, s: &StageStats) {
    eprintln!(
        "[summary] {stage:<9} batches={} input={} output={} bytes={} isolated_batches={} salvaged={} rejected={} \
         isolation_calls={} budget_exhausted={} delivery_retries={} wasm_mem_peak_mb={:.1}",
        s.batches.get(),
        s.input_records.get(),
        s.output_records.get(),
        s.input_bytes.get(),
        s.isolated_batches.get(),
        s.salvaged_records.get(),
        s.rejected_records.get(),
        s.isolation_calls.get(),
        s.budget_exhausted.get(),
        s.delivery_retries.get(),
        s.wasm_mem_peak.get() as f64 / 1_048_576.0,
    );
}

pub fn print_dead_letter_total() {
    eprintln!(
        "[summary] dead-letter records={}",
        dead_letter::total_records()
    );
}
