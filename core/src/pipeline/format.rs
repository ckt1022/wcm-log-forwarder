// Format 階段：plugin 回傳 `[4-byte LE 長度][資料]` 的 frame，一筆 entry 對應一個 frame。

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, RwLock};

use wasmtime::Store;

use super::runner::{StageBackend, StageRunner};
use super::{FormattedBatch, ParsedBatch, entry_record};
use crate::config::AppConfig;
use crate::dead_letter;
use crate::isolation::ProbeOutcome;
use crate::metrics::StageStats;
use crate::plugin::format_bindings::FormatPlugin;
use crate::plugin::local::log_process::pipeline_process::LogEntry;
use crate::plugin::{self, HostState, SharedPlugin};

struct FormatBackend {
    shared: SharedPlugin,
    mem_limit_bytes: usize,
}

struct FormatInstance {
    store: Store<HostState>,
    plugin: FormatPlugin,
}

/// 解析 plugin 回傳的 frame。
fn decode_frames(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut lines = Vec::new();
    let mut pos = 0;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        pos += 4;
        if pos + len > bytes.len() {
            return Err(format!(
                "malformed frame: declared {len} bytes but only {} remain",
                bytes.len() - pos
            ));
        }
        lines.push(bytes[pos..pos + len].to_vec());
        pos += len;
    }
    Ok(lines)
}

impl StageBackend for FormatBackend {
    type Item = LogEntry;
    type Output = Vec<u8>;
    type Instance = FormatInstance;

    fn label(&self) -> &'static str {
        "format"
    }

    fn plugin_version(&self) -> u64 {
        self.shared.read().unwrap().version
    }

    fn instantiate(&mut self) -> wasmtime::Result<FormatInstance> {
        let slot = self.shared.read().unwrap();
        let mut store = plugin::new_store(self.mem_limit_bytes, plugin::plugin_wasi_ctx(), None);
        let plugin = FormatPlugin::instantiate(&mut store, &slot.component, &slot.linker)?;
        Ok(FormatInstance { store, plugin })
    }

    fn invoke(
        &mut self,
        inst: &mut FormatInstance,
        items: &[LogEntry],
        ticks: u64,
    ) -> ProbeOutcome<Vec<u8>> {
        inst.store.set_epoch_deadline(ticks);
        match inst.plugin.call_format(&mut inst.store, items) {
            Ok(Ok(bytes)) => match decode_frames(&bytes) {
                Ok(lines) if lines.len() == items.len() => ProbeOutcome::Accepted(lines),
                Ok(lines) => ProbeOutcome::Rejected(format!(
                    "plugin returned {} frames for {} entries",
                    lines.len(),
                    items.len()
                )),
                Err(e) => ProbeOutcome::Rejected(e),
            },
            Ok(Err(e)) => ProbeOutcome::Rejected(format!("{e:?}")),
            Err(e) => ProbeOutcome::Trapped(e.to_string()),
        }
    }

    fn memory_peak(&self, inst: &FormatInstance) -> usize {
        inst.store.data().limiter.wasm_mem_peak
    }
}

pub fn run(
    rx: Receiver<ParsedBatch>,
    tx: SyncSender<FormattedBatch>,
    shared: SharedPlugin,
    cfg: Arc<RwLock<AppConfig>>,
    stats: Arc<StageStats>,
) -> wasmtime::Result<()> {
    let (ticks, reuse, mem_limit_bytes, max_chunk) = {
        let c = cfg.read().unwrap();
        (
            plugin::ticks_for(c.batch.stage_timeout()),
            c.batch.plugin_instance_reuse,
            c.batch.mem_limit_bytes(),
            c.batch.max_format_chunk,
        )
    };
    let backend = FormatBackend {
        shared,
        mem_limit_bytes,
    };
    let mut runner = StageRunner::new(backend, ticks, reuse, stats)?;

    while let Ok(mut batch) = rx.recv() {
        let mut lines = Vec::with_capacity(batch.entries.len());
        let mut targettags = Vec::with_capacity(batch.entries.len());

        for (chunk_index, chunk) in batch.entries.chunks(max_chunk).enumerate() {
            let offset = chunk_index * max_chunk;
            let report = runner.process(batch.seq, chunk);
            dead_letter::write_rejected("format", batch.seq, chunk.len(), &report.rejected, |i| {
                entry_record(&chunk[i])
            });
            for (range, output) in report.accepted {
                for (index, line) in range.zip(output) {
                    lines.push(line);
                    targettags.push(std::mem::take(&mut batch.targettags[offset + index]));
                }
            }
        }

        runner.stats.output_records.add(lines.len() as u64);
        if !lines.is_empty() {
            if let Err(e) = tx.send(FormattedBatch { lines, targettags }) {
                dead_letter::write(
                    &format!("[format] 下游已關閉 batch={}", batch.seq),
                    e.0.lines
                        .iter()
                        .map(|l| String::from_utf8_lossy(l).into_owned()),
                );
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    #[test]
    fn decode_returns_each_frame() {
        let mut bytes = frame("a");
        bytes.extend(frame("bcd"));
        assert_eq!(
            decode_frames(&bytes).unwrap(),
            vec![b"a".to_vec(), b"bcd".to_vec()]
        );
    }

    #[test]
    fn decode_rejects_truncated_frame() {
        let mut bytes = frame("abcd");
        bytes.truncate(6);
        assert!(
            decode_frames(&bytes)
                .unwrap_err()
                .contains("malformed frame")
        );
    }
}
