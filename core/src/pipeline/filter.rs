// Filter 階段：plugin 回傳每筆 keep／drop 的決策，host 依 id 套用。

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, RwLock};

use wasmtime::Store;

use super::runner::{StageBackend, StageRunner};
use super::{ParsedBatch, dead_letter_parsed, entry_record};
use crate::config::AppConfig;
use crate::dead_letter;
use crate::isolation::ProbeOutcome;
use crate::metrics::StageStats;
use crate::plugin::filter_bindings::ReductionPlugin as FilterPlugin;
use crate::plugin::local::log_process::pipeline_process::{FilterResult, LogEntry};
use crate::plugin::{self, HostState, SharedPlugin};

struct FilterBackend {
    shared: SharedPlugin,
    mem_limit_bytes: usize,
}

struct FilterInstance {
    store: Store<HostState>,
    plugin: FilterPlugin,
}

impl StageBackend for FilterBackend {
    type Item = LogEntry;
    type Output = FilterResult;
    type Instance = FilterInstance;

    fn label(&self) -> &'static str {
        "filter"
    }

    fn plugin_version(&self) -> u64 {
        self.shared.read().unwrap().version
    }

    fn instantiate(&mut self) -> wasmtime::Result<FilterInstance> {
        let slot = self.shared.read().unwrap();
        let mut store = plugin::new_store(self.mem_limit_bytes, plugin::plugin_wasi_ctx(), None);
        let plugin = FilterPlugin::instantiate(&mut store, &slot.component, &slot.linker)?;
        Ok(FilterInstance { store, plugin })
    }

    fn invoke(
        &mut self,
        inst: &mut FilterInstance,
        items: &[LogEntry],
        ticks: u64,
    ) -> ProbeOutcome<FilterResult> {
        inst.store.set_epoch_deadline(ticks);
        match inst.plugin.call_filter(&mut inst.store, items) {
            Ok(Ok(results)) if results.len() == items.len() => ProbeOutcome::Accepted(results),
            Ok(Ok(results)) => ProbeOutcome::Rejected(format!(
                "plugin returned {} results for {} entries",
                results.len(),
                items.len()
            )),
            Ok(Err(e)) => ProbeOutcome::Rejected(format!("{e:?}")),
            Err(e) => ProbeOutcome::Trapped(e.to_string()),
        }
    }

    fn memory_peak(&self, inst: &FilterInstance) -> usize {
        inst.store.data().limiter.wasm_mem_peak
    }
}

pub fn run(
    rx: Receiver<ParsedBatch>,
    tx: SyncSender<ParsedBatch>,
    shared: SharedPlugin,
    cfg: Arc<RwLock<AppConfig>>,
    stats: Arc<StageStats>,
) -> wasmtime::Result<()> {
    let (ticks, reuse, mem_limit_bytes) = {
        let c = cfg.read().unwrap();
        (
            plugin::ticks_for(c.batch.stage_timeout()),
            c.batch.plugin_instance_reuse,
            c.batch.mem_limit_bytes(),
        )
    };
    let backend = FilterBackend {
        shared,
        mem_limit_bytes,
    };
    let mut runner = StageRunner::new(backend, ticks, reuse, stats)?;

    while let Ok(batch) = rx.recv() {
        let report = runner.process(batch.seq, &batch.entries);
        dead_letter::write_rejected(
            "filter",
            batch.seq,
            batch.entries.len(),
            &report.rejected,
            |i| entry_record(&batch.entries[i]),
        );

        let keep: HashSet<u64> = report
            .accepted
            .iter()
            .flat_map(|(_, r)| r)
            .filter(|r| r.keep)
            .map(|r| r.id)
            .collect();
        let ParsedBatch {
            entries,
            targettags,
            seq,
        } = batch;
        let (entries, targettags): (Vec<LogEntry>, Vec<String>) = entries
            .into_iter()
            .zip(targettags)
            .filter(|(e, _)| keep.contains(&e.id))
            .unzip();

        runner.stats.output_records.add(entries.len() as u64);
        if !entries.is_empty() {
            if let Err(e) = tx.send(ParsedBatch {
                entries,
                targettags,
                seq,
            }) {
                dead_letter_parsed("filter", e.0);
                break;
            }
        }
    }
    Ok(())
}
