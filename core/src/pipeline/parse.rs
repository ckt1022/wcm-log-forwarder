// Parse 階段：dispatcher 組成處理批次，worker 呼叫 plugin 並分配 host id。

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use wasmtime::Store;

use super::runner::{StageBackend, StageRunner};
use super::{ParsedBatch, dead_letter_parsed};
use crate::config::AppConfig;
use crate::dead_letter;
use crate::isolation::{self, ProbeOutcome};
use crate::metrics::StageStats;
use crate::plugin::local::log_process::pipeline_process::{LogEntry, ParsedEntry};
use crate::plugin::{self, HostState, ParserPlugin, SharedPlugin};
use crate::spawn_named;

/// host id 的批次跨距：id = seq * ID_STRIDE + 原批次 index（單批上限 100,000 行）。
const ID_STRIDE: u64 = 100_000;

/// dispatcher→worker 的佇列長度（批）。
const WORK_QUEUE_BATCHES: usize = 64;

/// dispatcher 交給 worker 的工作單元。
struct WorkBatch {
    lines: Vec<String>,
    bytes: usize,
    seq: u64,
}

/// 處理批次的送出條件（每次迴圈由可熱更新的設定算出）。
#[derive(Clone, Copy, Debug)]
struct DispatchParams {
    max_wait: Duration,
    max_bytes: usize,
    max_records: usize,
}

impl DispatchParams {
    fn from_config(c: &AppConfig) -> Self {
        DispatchParams {
            max_wait: Duration::from_millis(c.batch.micro_batch_max_wait_ms),
            max_bytes: c.batch.micro_batch_max_bytes_kib * 1024,
            max_records: c.batch.micro_batch_records,
        }
    }
}

/// dispatcher 正在累積的批次。
struct PendingBatch {
    lines: Vec<String>,
    bytes: usize,
    first_arrival: Instant,
}

impl PendingBatch {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            first_arrival: Instant::now(),
        }
    }

    fn take(&mut self, seq: u64) -> WorkBatch {
        let batch = WorkBatch {
            lines: std::mem::take(&mut self.lines),
            bytes: self.bytes,
            seq,
        };
        self.bytes = 0;
        batch
    }
}

/// 達到位元組上限、筆數上限或等待上限（自第一筆到達起算），任一成立就送出。
/// 參數來源抽成 closure 方便測試。worker 只有一個，因此 seq 與 host id 的順序與輸入一致。
fn dispatch_loop(
    rx: Receiver<String>,
    tx: SyncSender<WorkBatch>,
    mut params: impl FnMut() -> DispatchParams,
) {
    fn send(pending: &mut PendingBatch, seq: &mut u64, tx: &SyncSender<WorkBatch>) -> bool {
        match tx.send(pending.take(*seq)) {
            Ok(()) => {
                *seq += 1;
                true
            }
            Err(e) => {
                dead_letter::write(
                    &format!("[parse] worker 已關閉 batch={}", e.0.seq),
                    e.0.lines,
                );
                false
            }
        }
    }

    let mut pending = PendingBatch::new();
    let mut seq: u64 = 0;

    loop {
        let DispatchParams {
            max_wait,
            max_bytes,
            max_records,
        } = params();
        let timeout = if pending.lines.is_empty() {
            max_wait
        } else {
            max_wait.saturating_sub(pending.first_arrival.elapsed())
        };

        match rx.recv_timeout(timeout) {
            Ok(line) => {
                if !pending.lines.is_empty()
                    && pending.bytes + line.len() > max_bytes
                    && !send(&mut pending, &mut seq, &tx)
                {
                    dead_letter::write("[parse] worker 已關閉", [line]);
                    break;
                }
                if pending.lines.is_empty() {
                    pending.first_arrival = Instant::now();
                }
                pending.bytes += line.len();
                pending.lines.push(line);
                if pending.lines.len() >= max_records && !send(&mut pending, &mut seq, &tx) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !pending.lines.is_empty() && !send(&mut pending, &mut seq, &tx) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !pending.lines.is_empty() {
                    send(&mut pending, &mut seq, &tx);
                }
                break;
            }
        }
    }
}

struct ParseBackend {
    shared: SharedPlugin,
    mem_limit_bytes: usize,
}

struct ParseInstance {
    store: Store<HostState>,
    plugin: ParserPlugin,
}

impl StageBackend for ParseBackend {
    type Item = String;
    type Output = ParsedEntry;
    type Instance = ParseInstance;

    fn label(&self) -> &'static str {
        "parse"
    }

    fn plugin_version(&self) -> u64 {
        self.shared.read().unwrap().version
    }

    fn instantiate(&mut self) -> wasmtime::Result<ParseInstance> {
        let slot = self.shared.read().unwrap();
        let mut store = plugin::new_store(self.mem_limit_bytes, plugin::plugin_wasi_ctx(), None);
        let plugin = ParserPlugin::instantiate(&mut store, &slot.component, &slot.linker)?;
        Ok(ParseInstance { store, plugin })
    }

    fn invoke(
        &mut self,
        inst: &mut ParseInstance,
        items: &[String],
        ticks: u64,
    ) -> ProbeOutcome<ParsedEntry> {
        plugin::arm_deadline(&mut inst.store, ticks);
        match inst.plugin.call_parse(&mut inst.store, items) {
            Ok(Ok(parsed)) => ProbeOutcome::Accepted(parsed),
            Ok(Err(e)) => ProbeOutcome::Rejected(format!("{e:?}")),
            Err(e) => ProbeOutcome::Trapped(e.to_string()),
        }
    }

    fn memory_peak(&self, inst: &ParseInstance) -> usize {
        inst.store.data().limiter.wasm_mem_peak
    }
}

/// 處理一批：拒絕的行寫入 dead-letter，其餘依原批次 index 分配 id。
fn process_batch(runner: &mut StageRunner<ParseBackend>, work: WorkBatch) -> Option<ParsedBatch> {
    let report = runner.process(work.seq, &work.lines);
    runner.stats.input_bytes.add(work.bytes as u64);
    dead_letter::write_rejected("parse", work.seq, work.lines.len(), &report.rejected, |i| {
        work.lines[i].clone()
    });

    let (entries, targettags): (Vec<LogEntry>, Vec<String>) =
        isolation::assign_ids(work.seq, ID_STRIDE, work.lines.len(), report.accepted)
            .into_iter()
            .map(|(id, p)| {
                let entry = LogEntry {
                    id,
                    timestamp: p.timestamp,
                    level: p.level,
                    message: p.message,
                    tags: p.tags,
                };
                (entry, p.targettag)
            })
            .unzip();

    runner.stats.output_records.add(entries.len() as u64);
    (!entries.is_empty()).then_some(ParsedBatch {
        entries,
        targettags,
        seq: work.seq,
    })
}

/// 啟動 worker，並在目前的執行緒跑 dispatcher。
pub fn run(
    rx: Receiver<String>,
    tx: SyncSender<ParsedBatch>,
    shared: SharedPlugin,
    cfg: Arc<RwLock<AppConfig>>,
    stats: Arc<StageStats>,
) -> wasmtime::Result<()> {
    let (ticks, reuse, mem_limit_bytes) = {
        let c = cfg.read().unwrap();
        (
            plugin::ticks_for(c.batch.stage_timeout()),
            c.batch.instance_policy(),
            c.batch.mem_limit_bytes(),
        )
    };
    let (tx_work, rx_work) = sync_channel::<WorkBatch>(WORK_QUEUE_BATCHES);

    let worker = spawn_named("parse-w0", move || -> wasmtime::Result<()> {
        let mut runner = StageRunner::new(
            ParseBackend {
                shared,
                mem_limit_bytes,
            },
            ticks,
            reuse,
            stats,
        )?;
        while let Ok(work) = rx_work.recv() {
            if let Some(batch) = process_batch(&mut runner, work) {
                if let Err(e) = tx.send(batch) {
                    dead_letter_parsed("parse", e.0);
                    break;
                }
            }
        }
        Ok(())
    });

    dispatch_loop(rx, tx_work, || {
        DispatchParams::from_config(&cfg.read().unwrap())
    });
    worker
        .join()
        .map_err(|_| wasmtime::Error::msg("parse worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{FsGrant, Stage, StageGrant};
    use crate::isolation::RejectReason;
    use std::thread;

    fn plugin_path(name: &str) -> String {
        format!("{}/plugin/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn setup_error_file() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let p = std::env::temp_dir().join(format!("wcm-test-error-{}.txt", std::process::id()));
            // SAFETY: 只在測試初始化時設定一次
            unsafe { std::env::set_var("WCM_ERROR_FILE", p) };
        });
    }

    fn real_runner(reuse: bool) -> StageRunner<ParseBackend> {
        setup_error_file();
        plugin::spawn_epoch_ticker();
        let shared = plugin::new_shared_runtime(
            &plugin_path("parse.wasm"),
            Stage::Parse,
            StageGrant::default(),
        )
        .unwrap();
        StageRunner::new(
            ParseBackend {
                shared,
                mem_limit_bytes: 64 * 1024 * 1024,
            },
            100_000,
            crate::config::InstancePolicy {
                reuse,
                recycle_bytes: usize::MAX,
                bisection: true,
            },
            Arc::default(),
        )
        .unwrap()
    }

    fn work(lines: Vec<String>, seq: u64) -> WorkBatch {
        let bytes = lines.iter().map(String::len).sum();
        WorkBatch { lines, bytes, seq }
    }

    fn good(i: usize) -> String {
        format!("{{\"ts\":\"t\",\"level\":\"warn\",\"msg\":\"m{i}\"}}")
    }

    fn mixed_lines() -> (Vec<String>, Vec<usize>) {
        let bad: Vec<usize> = (0..40).filter(|i| i % 7 == 3).collect();
        let lines = (0..40)
            .map(|i| {
                if bad.contains(&i) {
                    format!("{{\"ts\":\"t\",\"level\":\"warn\",\"msg\":\"broken-{i}\"")
                } else {
                    good(i)
                }
            })
            .collect();
        (lines, bad)
    }

    #[test]
    fn real_parser_bisection_salvages_valid_records_in_order() {
        let mut runner = real_runner(true);
        let (lines, bad) = mixed_lines();
        let batch = process_batch(&mut runner, work(lines, 5)).expect("salvaged batch");
        let want: Vec<usize> = (0..40).filter(|i| !bad.contains(i)).collect();
        assert_eq!(
            batch.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            want.iter()
                .map(|&i| 5 * ID_STRIDE + i as u64)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            batch
                .entries
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>(),
            want.iter().map(|i| format!("m{i}")).collect::<Vec<_>>()
        );
        assert_eq!(batch.targettags.len(), batch.entries.len());
        assert_eq!(runner.stats.rejected_records.get(), bad.len() as u64);
        assert_eq!(runner.stats.salvaged_records.get(), want.len() as u64);
        assert_eq!(runner.stats.isolated_batches.get(), 1);
    }

    #[test]
    fn real_parser_clean_batch_in_reuse_and_discard_modes() {
        for reuse in [true, false] {
            let mut runner = real_runner(reuse);
            for seq in 0..3 {
                let batch =
                    process_batch(&mut runner, work((0..10).map(good).collect(), seq)).unwrap();
                assert_eq!(batch.entries.len(), 10);
                assert_eq!(batch.entries[9].id, seq * ID_STRIDE + 9);
            }
            assert_eq!(runner.stats.isolated_batches.get(), 0);
        }
    }

    #[test]
    fn real_parser_rejects_every_malformed_line() {
        let mut runner = real_runner(true);
        let report = runner.process(1, &["not json".to_string(), "{broken".to_string()]);
        assert!(report.accepted.is_empty());
        assert_eq!(
            report.rejected,
            vec![
                (0, RejectReason::PluginError),
                (1, RejectReason::PluginError)
            ]
        );
    }

    /// 受限 linker 的第二層防線：即使跳過載入檢查，帶 filesystem import 的
    /// component 在 instantiate_pre 時也找不到實作。
    #[test]
    fn restricted_linker_cannot_satisfy_filesystem_import() {
        let engine = plugin::engine();
        let component =
            wasmtime::component::Component::from_file(engine, plugin_path("fault/parse.wasm"))
                .unwrap();
        let mut linker: wasmtime::component::Linker<HostState> =
            wasmtime::component::Linker::new(engine);
        crate::capability::add_wasi_to_linker_sync(&mut linker, StageGrant::default()).unwrap();
        let err = linker
            .instantiate_pre(&component)
            .err()
            .expect("must fail")
            .to_string();
        assert!(err.contains("wasi:filesystem"), "{err}");
        // 明確授權 inert filesystem 後，filesystem 這一項就能連結（仍然沒有任何 preopen）
        let mut linker2: wasmtime::component::Linker<HostState> =
            wasmtime::component::Linker::new(engine);
        let g = StageGrant {
            filesystem: FsGrant::Inert,
            ..Default::default()
        };
        crate::capability::add_wasi_to_linker_sync(&mut linker2, g).unwrap();
        let after = linker2
            .instantiate_pre(&component)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!after.contains("wasi:filesystem"), "{after}");
    }

    fn params(max_records: usize, wait_ms: u64) -> DispatchParams {
        DispatchParams {
            max_wait: Duration::from_millis(wait_ms),
            max_bytes: 1024 * 1024,
            max_records,
        }
    }

    #[test]
    fn dispatcher_flushes_on_record_count_in_order() {
        let (tx_raw, rx_raw) = sync_channel::<String>(100);
        let (tx_w, rx_w) = sync_channel::<WorkBatch>(100);
        for i in 0..10 {
            tx_raw.send(format!("l{i}")).unwrap();
        }
        drop(tx_raw);
        dispatch_loop(rx_raw, tx_w, || params(4, 60_000));
        let got: Vec<WorkBatch> = rx_w.try_iter().collect();
        assert_eq!(
            got.iter().map(|w| w.lines.len()).collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
        assert_eq!(got.iter().map(|w| w.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
        let all: Vec<String> = got.into_iter().flat_map(|w| w.lines).collect();
        assert_eq!(all, (0..10).map(|i| format!("l{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn dispatcher_flushes_on_byte_cap() {
        let (tx_raw, rx_raw) = sync_channel::<String>(100);
        let (tx_w, rx_w) = sync_channel::<WorkBatch>(100);
        for _ in 0..5 {
            tx_raw.send("x".repeat(400)).unwrap();
        }
        drop(tx_raw);
        dispatch_loop(rx_raw, tx_w, || DispatchParams {
            max_bytes: 1000,
            ..params(1000, 60_000)
        });
        let sizes: Vec<usize> = rx_w.try_iter().map(|w| w.lines.len()).collect();
        assert_eq!(sizes, vec![2, 2, 1]);
    }

    #[test]
    fn dispatcher_flushes_on_wait_from_first_record() {
        let (tx_raw, rx_raw) = sync_channel::<String>(100);
        let (tx_w, rx_w) = sync_channel::<WorkBatch>(100);
        let h = thread::spawn(move || dispatch_loop(rx_raw, tx_w, || params(1000, 30)));
        // 先閒置超過等待上限：計時必須從第一筆起算
        thread::sleep(Duration::from_millis(80));
        let t0 = Instant::now();
        for i in 0..3 {
            tx_raw.send(format!("l{i}")).unwrap();
        }
        let wb = rx_w
            .recv_timeout(Duration::from_secs(2))
            .expect("time trigger");
        let waited = t0.elapsed();
        assert_eq!(wb.lines.len(), 3, "閒置後第一筆不應被單獨送出");
        assert!(waited >= Duration::from_millis(25), "waited {waited:?}");
        assert!(waited < Duration::from_millis(500), "waited {waited:?}");
        drop(tx_raw);
        h.join().unwrap();
    }
}
