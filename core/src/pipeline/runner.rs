// 四個階段共用的 plugin 執行邏輯：instance 管理、失敗後重試、二分隔離與統計。
// 各階段只需實作 StageBackend（如何建立 instance、如何呼叫 plugin）。

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use crate::config::InstancePolicy;
use crate::isolation::{
    self, FailureKind, IsolationLimits, IsolationReport, ProbeOutcome, RejectReason,
};
use crate::metrics::StageStats;

/// instance 池的目標大小：一個使用中、一個備用，trap 後可立即接手。
const POOL_TARGET_SIZE: usize = 2;

/// trap 後在新 instance 上重試的次數；仍失敗才進入二分。
const MAX_RETRIES: u32 = 1;

/// trap 探測的 epoch deadline 下限（10 tick ≈ 100 ms）。
const PROBE_MIN_TICKS: u64 = 10;

pub trait StageBackend {
    type Item;
    type Output;
    type Instance;

    fn label(&self) -> &'static str;
    /// 目前的 plugin 版本；變動時整個 instance 池會換成新版。
    fn plugin_version(&self) -> u64;
    fn instantiate(&mut self) -> wasmtime::Result<Self::Instance>;
    fn invoke(
        &mut self,
        instance: &mut Self::Instance,
        items: &[Self::Item],
        deadline_ticks: u64,
    ) -> ProbeOutcome<Self::Output>;
    fn memory_peak(&self, instance: &Self::Instance) -> usize;

    fn limits(&self) -> IsolationLimits {
        isolation::DEFAULT_LIMITS
    }
}

pub struct StageRunner<B: StageBackend> {
    pub backend: B,
    pub stats: Arc<StageStats>,
    ready: VecDeque<B::Instance>,
    deadline_ticks: u64,
    policy: InstancePolicy,
    version: u64,
}

impl<B: StageBackend> StageRunner<B> {
    /// 建立 runner 並預熱 instance；一個都建不起來時回傳錯誤。
    pub fn new(
        mut backend: B,
        deadline_ticks: u64,
        policy: InstancePolicy,
        stats: Arc<StageStats>,
    ) -> wasmtime::Result<Self> {
        let first = backend.instantiate()?;
        let version = backend.plugin_version();
        let mut runner = Self {
            backend,
            stats,
            ready: VecDeque::from([first]),
            deadline_ticks,
            policy,
            version,
        };
        runner.replenish();
        Ok(runner)
    }

    fn replenish(&mut self) {
        while self.ready.len() < POOL_TARGET_SIZE {
            match self.backend.instantiate() {
                Ok(instance) => self.ready.push_back(instance),
                Err(e) => {
                    eprintln!("[{}] replenish failed: {e}", self.backend.label());
                    break;
                }
            }
        }
    }

    fn acquire(&mut self) -> wasmtime::Result<B::Instance> {
        match self.ready.pop_front() {
            Some(instance) => Ok(instance),
            None => self.backend.instantiate(),
        }
    }

    /// 呼叫成功或 plugin 正常回傳錯誤後，依設定放回或丟棄。
    /// 線性記憶體只會成長不會縮小，超過門檻代表插件可能沒有釋放記憶體，在用完上限前換新。
    fn release(&mut self, instance: B::Instance) {
        let memory = self.backend.memory_peak(&instance);
        self.stats.wasm_mem_peak.update_max(memory as u64);
        if self.policy.reuse && memory < self.policy.recycle_bytes {
            self.ready.push_front(instance);
            return;
        }
        if self.policy.reuse {
            self.stats.recycled_instances.add(1);
        }
        drop(instance);
        self.replenish();
    }

    /// instance 已污染（trap／逾時），丟棄並補充。
    fn discard(&mut self, instance: B::Instance) {
        drop(instance);
        self.replenish();
    }

    /// 熱更換：先確認新版能建立 instance，再替換整個池；失敗時沿用舊版。
    fn check_reload(&mut self) {
        let latest = self.backend.plugin_version();
        if latest == self.version {
            return;
        }
        self.version = latest;
        match self.backend.instantiate() {
            Ok(fresh) => {
                self.ready.clear();
                self.ready.push_back(fresh);
                self.replenish();
                eprintln!("[{}] plugin hot-swapped → v{latest}", self.backend.label());
            }
            Err(e) => eprintln!(
                "[{}] plugin reload failed（沿用舊版）: {e}",
                self.backend.label()
            ),
        }
    }

    /// 處理一批資料。回傳的 report 涵蓋每一筆輸入：不是在 accepted 就是在 rejected。
    /// host 端的 panic 也在這裡攔下，讓呼叫端能把這批資料寫入 dead-letter。
    pub fn process(&mut self, seq: u64, items: &[B::Item]) -> IsolationReport<B::Output> {
        let result = panic::catch_unwind(AssertUnwindSafe(|| self.process_batch(seq, items)));
        self.stats.completed_records.add(items.len() as u64);
        result.unwrap_or_else(|_| {
            let report = IsolationReport::rejected_all(
                items.len(),
                RejectReason::HostPanic,
                "host panic".to_string(),
            );
            self.record_isolation(seq, items.len(), "host-panic", &report);
            report
        })
    }

    fn process_batch(&mut self, seq: u64, items: &[B::Item]) -> IsolationReport<B::Output> {
        self.check_reload();
        self.stats.batches.add(1);
        self.stats.input_records.add(items.len() as u64);
        let label = self.backend.label();
        let mut last_error = String::from("acquire failed");

        for attempt in 0..=MAX_RETRIES {
            let mut instance = match self.acquire() {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[{label}] cannot acquire instance batch={seq}: {e}");
                    last_error = e.to_string();
                    continue;
                }
            };
            match self
                .backend
                .invoke(&mut instance, items, self.deadline_ticks)
            {
                ProbeOutcome::Accepted(output) => {
                    self.release(instance);
                    return IsolationReport::whole(items.len(), output);
                }
                ProbeOutcome::Rejected(message) => {
                    eprintln!("[{label}-error] batch={seq} {message}");
                    return self.isolate(
                        seq,
                        items,
                        Some(instance),
                        FailureKind::Rejected,
                        message,
                    );
                }
                ProbeOutcome::GaveUp(message) => {
                    self.release(instance);
                    let report = IsolationReport::rejected_all(
                        items.len(),
                        RejectReason::DeliveryFailed,
                        message,
                    );
                    self.record_isolation(seq, items.len(), "delivery-failed", &report);
                    return report;
                }
                ProbeOutcome::Trapped(message) => {
                    eprintln!(
                        "[{label}-trap {}/{}] batch={seq}: {message}",
                        attempt + 1,
                        MAX_RETRIES + 1
                    );
                    last_error = message;
                    self.discard(instance);
                }
            }
        }
        self.isolate(seq, items, None, FailureKind::Trapped, last_error)
    }

    fn isolate(
        &mut self,
        seq: u64,
        items: &[B::Item],
        first: Option<B::Instance>,
        kind: FailureKind,
        first_error: String,
    ) -> IsolationReport<B::Output> {
        let n = items.len();
        if !self.policy.bisection {
            // 消融實驗：不二分，整批拒絕
            if let Some(instance) = first {
                self.release(instance);
            }
            let reason = match kind {
                FailureKind::Rejected => RejectReason::PluginError,
                FailureKind::Trapped => RejectReason::Poison,
            };
            let report = IsolationReport::rejected_all(n, reason, first_error);
            self.record_isolation(seq, n, "no-bisection", &report);
            return report;
        }
        let limits = self.backend.limits();
        let full_ticks = self.deadline_ticks;
        let mut current = first;

        let mut report = isolation::isolate(n, kind, limits, |range, trap_mode| {
            let mut instance = match current.take() {
                Some(i) => i,
                None => match self.acquire() {
                    Ok(i) => i,
                    Err(e) => return ProbeOutcome::Trapped(format!("acquire failed: {e}")),
                },
            };
            let ticks = if trap_mode {
                isolation::probe_deadline_ticks(full_ticks, n, range.len(), PROBE_MIN_TICKS)
            } else {
                full_ticks
            };
            let outcome = self.backend.invoke(&mut instance, &items[range], ticks);
            if matches!(outcome, ProbeOutcome::Trapped(_)) {
                self.discard(instance);
            } else {
                current = Some(instance);
            }
            outcome
        });
        if let Some(instance) = current.take() {
            self.release(instance);
        }
        report.last_error.get_or_insert(first_error);
        let reason = match kind {
            FailureKind::Rejected => "plugin-error",
            FailureKind::Trapped => "trap",
        };
        self.record_isolation(seq, n, reason, &report);
        report
    }

    fn record_isolation(
        &mut self,
        seq: u64,
        n: usize,
        reason: &str,
        report: &IsolationReport<B::Output>,
    ) {
        let salvaged = report.salvaged();
        self.stats.isolated_batches.add(1);
        self.stats.salvaged_records.add(salvaged as u64);
        self.stats
            .rejected_records
            .add(report.rejected.len() as u64);
        self.stats.isolation_calls.add(report.calls as u64);
        if report.budget_exhausted {
            self.stats.budget_exhausted.add(1);
        }
        // trap 訊息常為多行 backtrace，只取第一行
        let last_error = report
            .last_error
            .as_deref()
            .and_then(|m| m.lines().next())
            .unwrap_or("-");
        eprintln!(
            "[{}-isolate] batch={seq} reason={reason} lines={n} salvaged={salvaged} rejected={} calls={} trap_calls={} budget_exhausted={} last_error={last_error}",
            self.backend.label(),
            report.rejected.len(),
            report.calls,
            report.trap_calls,
            report.budget_exhausted,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    const POISON: u32 = 666;
    const BAD: u32 = 13;
    const HOST_BUG: u32 = 999;
    /// 測試用的換新門檻；Fake 處理到 GROW 時記憶體會超過它
    const RECYCLE_AT: usize = 1000;
    const GROW: u32 = 4242;

    /// 不需要 WASM 的假 backend：instance 記錄自己的編號，POISON 會 trap，BAD 會回傳錯誤。
    struct Fake {
        created: Rc<Cell<u32>>,
        version: Rc<Cell<u64>>,
        instances_used: Vec<(u32, u64)>,
        fail_instantiate: Rc<Cell<bool>>,
    }

    struct FakeInstance {
        id: u32,
        version: u64,
        poisoned: bool,
        memory: usize,
    }

    impl StageBackend for Fake {
        type Item = u32;
        type Output = u32;
        type Instance = FakeInstance;

        fn label(&self) -> &'static str {
            "fake"
        }
        fn plugin_version(&self) -> u64 {
            self.version.get()
        }
        fn instantiate(&mut self) -> wasmtime::Result<FakeInstance> {
            if self.fail_instantiate.get() {
                return Err(wasmtime::Error::msg("cannot instantiate"));
            }
            self.created.set(self.created.get() + 1);
            Ok(FakeInstance {
                id: self.created.get(),
                version: self.version.get(),
                poisoned: false,
                memory: 0,
            })
        }
        fn invoke(
            &mut self,
            inst: &mut FakeInstance,
            items: &[u32],
            _ticks: u64,
        ) -> ProbeOutcome<u32> {
            assert!(!inst.poisoned, "污染的 instance 不可再被呼叫");
            assert!(!items.contains(&HOST_BUG), "模擬 host 端的 bug");
            self.instances_used.push((inst.id, inst.version));
            if items.contains(&POISON) {
                inst.poisoned = true;
                return ProbeOutcome::Trapped("trap".into());
            }
            if items.contains(&BAD) {
                return ProbeOutcome::Rejected("bad".into());
            }
            if items.contains(&GROW) {
                inst.memory = RECYCLE_AT;
            }
            ProbeOutcome::Accepted(items.to_vec())
        }
        fn memory_peak(&self, inst: &FakeInstance) -> usize {
            inst.memory
        }
    }

    struct Handles {
        created: Rc<Cell<u32>>,
        version: Rc<Cell<u64>>,
        fail: Rc<Cell<bool>>,
    }

    fn runner(reuse: bool) -> (StageRunner<Fake>, Handles) {
        let h = Handles {
            created: Rc::default(),
            version: Rc::default(),
            fail: Rc::default(),
        };
        let backend = Fake {
            created: Rc::clone(&h.created),
            version: Rc::clone(&h.version),
            instances_used: Vec::new(),
            fail_instantiate: Rc::clone(&h.fail),
        };
        (
            StageRunner::new(
                backend,
                1000,
                InstancePolicy {
                    reuse,
                    recycle_bytes: RECYCLE_AT,
                    bisection: true,
                },
                Arc::default(),
            )
            .unwrap(),
            h,
        )
    }

    fn accepted(report: IsolationReport<u32>) -> Vec<u32> {
        report.accepted.into_iter().flat_map(|(_, v)| v).collect()
    }

    #[test]
    fn reuse_mode_keeps_the_same_instance_across_batches() {
        let (mut r, h) = runner(true);
        for seq in 0..5 {
            assert_eq!(accepted(r.process(seq, &[1, 2])), vec![1, 2]);
        }
        assert_eq!(h.created.get(), POOL_TARGET_SIZE as u32, "只有預熱時建立");
        assert!(
            r.backend
                .instances_used
                .iter()
                .all(|(id, _)| *id == r.backend.instances_used[0].0)
        );
    }

    #[test]
    fn instance_over_memory_threshold_is_replaced() {
        let (mut r, _) = runner(true);
        r.process(0, &[1]);
        r.process(1, &[GROW]);
        r.process(2, &[1]);
        let ids: Vec<u32> = r.backend.instances_used.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids[0], ids[1], "門檻以下沿用同一個 instance");
        assert_ne!(ids[1], ids[2], "超過門檻後換新");
        assert_eq!(r.stats.recycled_instances.get(), 1);
    }

    #[test]
    fn discard_mode_uses_a_new_instance_every_batch() {
        let (mut r, h) = runner(false);
        for seq in 0..5 {
            r.process(seq, &[1]);
        }
        assert_eq!(h.created.get(), POOL_TARGET_SIZE as u32 + 5);
        let ids: std::collections::HashSet<u32> =
            r.backend.instances_used.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn trap_rebuilds_before_retry_and_isolates_only_the_poison_record() {
        let (mut r, _) = runner(true);
        let items: Vec<u32> = (100..116)
            .map(|i| if i == 109 { POISON } else { i })
            .collect();
        let report = r.process(1, &items);
        assert_eq!(report.rejected, vec![(9, RejectReason::Poison)]);
        let want: Vec<u32> = items.iter().copied().filter(|&v| v != POISON).collect();
        assert_eq!(accepted(report), want, "順序維持");
        assert_eq!(r.stats.rejected_records.get(), 1);
        assert_eq!(r.stats.salvaged_records.get(), 15);
        assert_eq!(r.stats.isolated_batches.get(), 1);
        // 下一批不會碰到污染的 instance（Fake 的 assert 會攔住）
        assert_eq!(accepted(r.process(2, &[1])), vec![1]);
    }

    #[test]
    fn plugin_error_isolates_without_retry() {
        let (mut r, _) = runner(true);
        let report = r.process(1, &[1, BAD, 3, 4]);
        assert_eq!(report.rejected, vec![(1, RejectReason::PluginError)]);
        assert_eq!(accepted(report), vec![1, 3, 4]);
        // 第一次整批失敗後直接二分：不重試整批
        assert_eq!(
            r.backend.instances_used.len(),
            1 + r.stats.isolation_calls.get() as usize
        );
    }

    #[test]
    fn without_bisection_the_whole_batch_is_rejected() {
        let (mut r, _) = runner(true);
        r.policy.bisection = false;
        let report = r.process(1, &[1, BAD, 3]);
        assert!(report.accepted.is_empty());
        assert_eq!(report.rejected.len(), 3);
        let report = r.process(2, &[1, POISON, 3]);
        assert!(
            report
                .rejected
                .iter()
                .all(|(_, why)| *why == RejectReason::Poison)
        );
        assert_eq!(report.rejected.len(), 3);
    }

    #[test]
    fn single_poison_record_is_tried_twice_then_rejected() {
        let (mut r, _) = runner(true);
        let report = r.process(1, &[POISON]);
        assert_eq!(report.rejected, vec![(0, RejectReason::Poison)]);
        assert_eq!(r.backend.instances_used.len(), 2);
    }

    #[test]
    fn hot_swap_applies_before_the_next_batch() {
        let (mut r, h) = runner(true);
        r.process(0, &[1]);
        h.version.set(7);
        r.process(1, &[1]);
        assert_eq!(r.backend.instances_used.last().unwrap().1, 7);
    }

    #[test]
    fn failed_hot_swap_keeps_old_instances_serving() {
        let (mut r, h) = runner(true);
        h.version.set(7);
        h.fail.set(true);
        assert_eq!(accepted(r.process(1, &[1])), vec![1]);
        assert_eq!(r.backend.instances_used.last().unwrap().1, 0);
    }

    #[test]
    fn host_panic_reports_the_batch_and_runner_keeps_working() {
        let (mut r, _) = runner(true);
        let report = r.process(1, &[1, HOST_BUG, 3]);
        assert_eq!(report.rejected.len(), 3);
        assert!(
            report
                .rejected
                .iter()
                .all(|(_, why)| *why == RejectReason::HostPanic)
        );
        assert_eq!(accepted(r.process(2, &[7])), vec![7]);
    }

    #[test]
    fn every_record_is_reported_when_no_instance_can_be_created() {
        let (mut r, h) = runner(false);
        h.fail.set(true);
        // 用掉預熱的 instance，之後完全無法建立
        r.process(0, &[1]);
        r.process(1, &[1]);
        let report = r.process(2, &[1, 2, 3, 4]);
        assert_eq!(
            report.rejected.len() + report.salvaged(),
            4,
            "沒有任何一筆憑空消失"
        );
    }
}
