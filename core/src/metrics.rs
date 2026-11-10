// 執行中的計數器與即時狀態輸出。
// 各階段只做 atomic 累加；狀態由獨立的執行緒定期輸出，不增加批次處理路徑上的 I/O。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{AppConfig, PipelineStages};
use crate::dead_letter;
use crate::spawn_named;

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn update_max(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// 單一階段的計數器；同一階段的所有 worker 共用。
#[derive(Default)]
pub struct StageStats {
    pub batches: Counter,
    /// 開始處理的筆數
    pub input_records: Counter,
    /// 處理完成的筆數（不論成功或拒絕）
    pub completed_records: Counter,
    pub output_records: Counter,
    pub input_bytes: Counter,
    pub isolated_batches: Counter,
    pub salvaged_records: Counter,
    pub rejected_records: Counter,
    pub isolation_calls: Counter,
    pub budget_exhausted: Counter,
    pub delivery_retries: Counter,
    /// router 無法分流、直接寫入 dead-letter 的筆數
    pub unroutable_records: Counter,
    pub wasm_mem_peak: Counter,
}

/// 整條管線的計數器。
#[derive(Default)]
pub struct Metrics {
    pub input_records: Counter,
    pub parse: Arc<StageStats>,
    pub filter: Arc<StageStats>,
    pub format: Arc<StageStats>,
    pub transport: Arc<StageStats>,
}

#[derive(Clone, Copy, Default)]
struct StageSnapshot {
    input: u64,
    completed: u64,
    output: u64,
    rejected: u64,
    retries: u64,
    unroutable: u64,
}

impl StageSnapshot {
    fn of(s: &StageStats) -> Self {
        Self {
            input: s.input_records.get(),
            completed: s.completed_records.get(),
            output: s.output_records.get(),
            rejected: s.rejected_records.get(),
            retries: s.delivery_retries.get(),
            unroutable: s.unroutable_records.get(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Snapshot {
    input: u64,
    parse: StageSnapshot,
    filter: StageSnapshot,
    format: StageSnapshot,
    transport: StageSnapshot,
    dead_letter: u64,
}

impl Metrics {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            input: self.input_records.get(),
            parse: StageSnapshot::of(&self.parse),
            filter: StageSnapshot::of(&self.filter),
            format: StageSnapshot::of(&self.format),
            transport: StageSnapshot::of(&self.transport),
            dead_letter: dead_letter::total_records(),
        }
    }
}

fn per_second(current: u64, previous: u64, elapsed: Duration) -> u64 {
    (current.saturating_sub(previous) as f64 / elapsed.as_secs_f64().max(0.001)).round() as u64
}

/// 組出一行狀態；完全閒置（沒有新資料也沒有積壓）時回傳 None，避免洗版。
/// q 是該階段尚未處理完的筆數（佇列中加上處理中）；targettag 分流到多個端點時，Transport 的 q 是估計值。
fn format_status(
    previous: &Snapshot,
    current: &Snapshot,
    elapsed: Duration,
    uptime: Duration,
    stages: &PipelineStages,
) -> Option<String> {
    let mut chain = vec![("parse", previous.parse, current.parse)];
    if stages.filter {
        chain.push(("filter", previous.filter, current.filter));
    }
    if stages.format {
        chain.push(("format", previous.format, current.format));
    }
    if stages.transport {
        chain.push(("transport", previous.transport, current.transport));
    }

    let mut active = current.input != previous.input || current.dead_letter != previous.dead_letter;
    let mut upstream_output = current.input;
    let mut parts = vec![format!(
        "input={}/s",
        per_second(current.input, previous.input, elapsed)
    )];
    for (name, before, now) in chain {
        let backlog = upstream_output.saturating_sub(now.completed + now.unroutable);
        active |= backlog > 0 || now.completed != before.completed || now.retries != before.retries;
        parts.push(format!(
            "{name} in={}/s out={}/s q={backlog}",
            per_second(now.input, before.input, elapsed),
            per_second(now.output, before.output, elapsed),
        ));
        upstream_output = now.output;
    }
    if !active {
        return None;
    }

    let rejected = current.parse.rejected
        + current.filter.rejected
        + current.format.rejected
        + current.transport.rejected;
    let sent = if stages.transport {
        current.transport.output
    } else {
        0
    };
    Some(format!(
        "[status] up={}s {} | sent={sent} rejected={rejected} retries={} dead_letter={}",
        uptime.as_secs(),
        parts.join(" | "),
        current.transport.retries,
        current.dead_letter,
    ))
}

/// 每 `status_interval_secs` 秒輸出一行狀態（0 為關閉，可熱更新）。
pub fn spawn_status_reporter(cfg: Arc<RwLock<AppConfig>>, metrics: Arc<Metrics>) {
    spawn_named("status", move || {
        let started = Instant::now();
        let mut previous = metrics.snapshot();
        let mut last = Instant::now();
        loop {
            let (interval, stages) = {
                let c = cfg.read().unwrap();
                (c.status_interval_secs, c.stages.clone())
            };
            thread::sleep(Duration::from_secs(interval.max(1)));
            let current = metrics.snapshot();
            let now = Instant::now();
            if interval > 0 {
                if let Some(line) =
                    format_status(&previous, &current, now - last, started.elapsed(), &stages)
                {
                    eprintln!("{line}");
                }
            }
            previous = current;
            last = now;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);
    const ALL: PipelineStages = PipelineStages {
        filter: true,
        format: true,
        transport: true,
    };

    fn stage(input: u64, completed: u64, output: u64) -> StageSnapshot {
        StageSnapshot {
            input,
            completed,
            output,
            ..Default::default()
        }
    }

    fn flowing() -> Snapshot {
        Snapshot {
            input: 1000,
            parse: stage(1000, 1000, 1000),
            filter: stage(1000, 1000, 400),
            format: stage(400, 400, 400),
            transport: stage(400, 400, 400),
            dead_letter: 0,
        }
    }

    #[test]
    fn reports_rates_per_stage_and_totals() {
        let line =
            format_status(&Snapshot::default(), &flowing(), SECOND, SECOND * 5, &ALL).unwrap();
        assert!(line.starts_with("[status] up=5s input=1000/s"), "{line}");
        assert!(line.contains("filter in=1000/s out=400/s q=0"), "{line}");
        assert!(line.contains("transport in=400/s out=400/s q=0"), "{line}");
        assert!(
            line.contains("sent=400 rejected=0 retries=0 dead_letter=0"),
            "{line}"
        );
    }

    #[test]
    fn rates_are_scaled_by_elapsed_time() {
        let line =
            format_status(&Snapshot::default(), &flowing(), SECOND * 2, SECOND, &ALL).unwrap();
        assert!(line.contains("input=500/s"), "{line}");
    }

    #[test]
    fn idle_pipeline_prints_nothing() {
        let same = flowing();
        assert!(format_status(&same, &same, SECOND, SECOND, &ALL).is_none());
    }

    #[test]
    fn stuck_stage_keeps_reporting_and_shows_where_data_waits() {
        let mut stuck = flowing();
        stuck.transport.completed = 100;
        let line = format_status(&stuck, &stuck, SECOND, SECOND, &ALL).expect("積壓時不可視為閒置");
        assert!(line.contains("transport in=0/s out=0/s q=300"), "{line}");
    }

    #[test]
    fn disabled_stages_are_left_out_of_the_chain() {
        let stages = PipelineStages {
            filter: false,
            format: true,
            transport: false,
        };
        let mut s = flowing();
        s.format = stage(1000, 1000, 1000);
        let line = format_status(&Snapshot::default(), &s, SECOND, SECOND, &stages).unwrap();
        assert!(
            !line.contains("filter") && !line.contains("transport"),
            "{line}"
        );
        assert!(line.contains("format in=1000/s out=1000/s q=0"), "{line}");
    }

    #[test]
    fn unroutable_records_do_not_count_as_backlog() {
        let mut s = flowing();
        s.transport.completed = 390;
        s.transport.unroutable = 10;
        let line = format_status(&Snapshot::default(), &s, SECOND, SECOND, &ALL).unwrap();
        assert!(line.contains("transport in=400/s out=400/s q=0"), "{line}");
    }
}
