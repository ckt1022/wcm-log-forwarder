// ── 四個階段共用的逐筆錯誤隔離（host 端二分法）──────────────────────────────
//
// 問題：plugin 的 WIT 簽名都是「一批進、一個 result 出」。批次中只要有一筆造成
// 錯誤，host 就只知道整批失敗。
//
// 做法：host 把失敗的批次遞迴切半重新呼叫：成功的半批保留，失敗的半批再切，
// 直到單筆為止；只有真正造成錯誤的資料會被拒絕，並寫入 dead-letter 檔案。
//
// 失敗分三類：
//   1. Rejected（plugin 正常回傳錯誤）：呼叫便宜、instance 未污染。
//      二分次數受 max_rejected_calls 限制（Transport 每次探測都是一個網路請求）。
//   2. Trapped（trap／逾時／記憶體上限）：每次探測都可能耗掉整個時限，
//      受「呼叫次數」與「深度」雙重上限保護；預算用完時，尚未定位的區段整段拒絕。
//      stage 另外依區段長度縮短探測時限，單一 poison record 的隔離時間約為 2T。
//   3. GaveUp（Transport 重試耗盡）：問題在端點而不在資料，繼續切半沒有意義，
//      這一段直接拒絕。
//
// 本模組只含純邏輯（以閉包注入「呼叫一段輸入」的動作），方便單元測試；
// instance 管理在 pipeline/runner.rs。

use std::ops::Range;

/// 單次探測（對一段連續輸入呼叫一次 plugin）的結果分類。
#[derive(Debug)]
pub enum ProbeOutcome<T> {
    /// plugin 回傳 Ok
    Accepted(Vec<T>),
    /// plugin 回傳錯誤（instance 仍可用）
    Rejected(String),
    /// trap／epoch timeout／OOM（instance 已污染，需丟棄）
    Trapped(String),
    /// Transport 重試耗盡：問題不在這批資料，不再切半
    GaveUp(String),
}

/// 失敗種類：決定二分時是否受 trap 預算限制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Rejected,
    Trapped,
}

/// 單筆（或預算耗盡時的一段）被拒絕的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// 單筆輸入讓 plugin 回傳錯誤
    PluginError,
    /// 單筆輸入讓 plugin trap／timeout（poison record）
    Poison,
    /// 隔離預算用完，整段未定位即拒絕
    BudgetExhausted,
    /// Transport 重試耗盡（端點無法使用）
    DeliveryFailed,
    /// host 端處理這批時發生 panic
    HostPanic,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::PluginError => "plugin-error",
            RejectReason::Poison => "poison(trap/timeout)",
            RejectReason::BudgetExhausted => "budget-exhausted",
            RejectReason::DeliveryFailed => "delivery-failed",
            RejectReason::HostPanic => "host-panic",
        }
    }
}

/// 二分的上限。
#[derive(Debug, Clone, Copy)]
pub struct IsolationLimits {
    /// 進入 trap 模式後最多再呼叫幾次 plugin（每次都可能耗掉一個 timeout）
    pub max_trap_calls: u32,
    /// trap 二分的最大遞迴深度（2^depth 筆以上的批次仍會被截斷）
    pub max_trap_depth: u32,
    /// plugin 回傳錯誤時最多呼叫幾次
    pub max_rejected_calls: u32,
}

/// 預設：32 次呼叫 ≈ 對 65,536 筆批次做一次完整的單筆定位（2 × log2）。
pub const DEFAULT_LIMITS: IsolationLimits = IsolationLimits {
    max_trap_calls: 32,
    max_trap_depth: 16,
    max_rejected_calls: u32::MAX,
};

/// 二分隔離的結果。`accepted` 依輸入順序排列，每個元素是
/// （該段在原批次的 index 範圍, plugin 對該段的輸出）。
#[derive(Debug)]
pub struct IsolationReport<T> {
    pub accepted: Vec<(Range<usize>, Vec<T>)>,
    /// （原批次 index, 拒絕原因），依輸入順序
    pub rejected: Vec<(usize, RejectReason)>,
    /// 本次隔離總共呼叫 plugin 的次數（不含最初失敗的整批呼叫）
    pub calls: u32,
    /// 其中處於 trap 模式的呼叫次數
    pub trap_calls: u32,
    /// trap 預算是否曾經用完
    pub budget_exhausted: bool,
    /// 最後一次探測失敗的訊息（plugin 錯誤或 trap 原因），供 log 診斷
    pub last_error: Option<String>,
}

impl<T> IsolationReport<T> {
    /// 整批第一次就成功時的結果。
    pub fn whole(len: usize, output: Vec<T>) -> Self {
        Self {
            accepted: vec![(0..len, output)],
            rejected: Vec::new(),
            calls: 0,
            trap_calls: 0,
            budget_exhausted: false,
            last_error: None,
        }
    }

    /// 整批直接拒絕（不探測）。
    pub fn rejected_all(len: usize, reason: RejectReason, message: String) -> Self {
        Self {
            accepted: Vec::new(),
            rejected: (0..len).map(|i| (i, reason.clone())).collect(),
            calls: 0,
            trap_calls: 0,
            budget_exhausted: false,
            last_error: Some(message),
        }
    }

    /// 被保留下來的輸入筆數
    pub fn salvaged(&self) -> usize {
        self.accepted.iter().map(|(r, _)| r.len()).sum()
    }
}

/// 對長度 `len` 的批次做二分隔離。呼叫前提：整批已經以 `first_failure` 失敗過。
///
/// `probe(range, trap_mode)` 對原批次的 `range` 子區段呼叫一次 plugin；
/// `trap_mode` 為 true 表示這段位於 trap 區段內，呼叫端可據此縮短 deadline
/// 或改用新 instance。
pub fn isolate<T, F>(
    len: usize,
    first_failure: FailureKind,
    limits: IsolationLimits,
    mut probe: F,
) -> IsolationReport<T>
where
    F: FnMut(Range<usize>, bool) -> ProbeOutcome<T>,
{
    let mut report = IsolationReport {
        accepted: Vec::new(),
        rejected: Vec::new(),
        calls: 0,
        trap_calls: 0,
        budget_exhausted: false,
        last_error: None,
    };
    if len == 0 {
        return report;
    }
    split(0..len, 0, first_failure, &limits, &mut probe, &mut report);
    report
}

/// 遞迴處理一段「已知失敗」的區段：先左半、後右半，確保輸出維持輸入順序。
fn split<T, F>(
    range: Range<usize>,
    depth: u32,
    kind: FailureKind,
    limits: &IsolationLimits,
    probe: &mut F,
    report: &mut IsolationReport<T>,
) where
    F: FnMut(Range<usize>, bool) -> ProbeOutcome<T>,
{
    // 單筆即為元兇，不需再探測
    if range.len() == 1 {
        let reason = match kind {
            FailureKind::Rejected => RejectReason::PluginError,
            FailureKind::Trapped => RejectReason::Poison,
        };
        report.rejected.push((range.start, reason));
        return;
    }

    let trap_mode = kind == FailureKind::Trapped;
    // 需要兩次探測（左右各一），預算不足或太深就整段拒絕
    let over_budget = if trap_mode {
        report.trap_calls + 2 > limits.max_trap_calls || depth >= limits.max_trap_depth
    } else {
        report.calls.saturating_add(2) > limits.max_rejected_calls
    };
    if over_budget {
        report.budget_exhausted = true;
        for i in range {
            report.rejected.push((i, RejectReason::BudgetExhausted));
        }
        return;
    }

    let mid = range.start + range.len() / 2;
    for half in [range.start..mid, mid..range.end] {
        if !trap_mode && report.calls >= limits.max_rejected_calls {
            report.budget_exhausted = true;
            report
                .rejected
                .extend(half.map(|i| (i, RejectReason::BudgetExhausted)));
            continue;
        }
        report.calls += 1;
        if trap_mode {
            report.trap_calls += 1;
        }
        match probe(half.clone(), trap_mode) {
            ProbeOutcome::Accepted(entries) => report.accepted.push((half, entries)),
            ProbeOutcome::Rejected(msg) => {
                report.last_error = Some(msg);
                split(
                    half,
                    depth + 1,
                    FailureKind::Rejected,
                    limits,
                    probe,
                    report,
                )
            }
            ProbeOutcome::Trapped(msg) => {
                report.last_error = Some(msg);
                split(half, depth + 1, FailureKind::Trapped, limits, probe, report)
            }
            ProbeOutcome::GaveUp(msg) => {
                report.last_error = Some(msg);
                report
                    .rejected
                    .extend(half.map(|i| (i, RejectReason::DeliveryFailed)));
            }
        }
    }
}

/// 依「原批次 index」產生 host id：`seq * stride + index`。
/// plugin 對某段回傳的筆數若多於輸入（非 1:1 parser），多出的部分改用
/// `seq * stride + len + k` 依序編號，保證同一批次內 id 唯一且遞增。
pub fn assign_ids<T>(
    seq: u64,
    stride: u64,
    batch_len: usize,
    accepted: Vec<(Range<usize>, Vec<T>)>,
) -> Vec<(u64, T)> {
    let mut out = Vec::new();
    let mut overflow: u64 = 0;
    for (range, entries) in accepted {
        let n = range.len();
        for (i, e) in entries.into_iter().enumerate() {
            let idx = if i < n {
                (range.start + i) as u64
            } else {
                let v = batch_len as u64 + overflow;
                overflow += 1;
                v
            };
            out.push((seq * stride + idx, e));
        }
    }
    out
}

/// trap 探測的 epoch deadline：依區段長度等比例縮短（2 倍安全係數），
/// 下限 `min_ticks`、上限為完整的 stage deadline。
/// 以單一 poison record 為例，各層探測時間為 T, T/2, T/4 ...，總和約 2T。
pub fn probe_deadline_ticks(
    full_ticks: u64,
    full_len: usize,
    probe_len: usize,
    min_ticks: u64,
) -> u64 {
    if full_len == 0 {
        return full_ticks;
    }
    let scaled = (full_ticks as u128 * 2 * probe_len as u128 / full_len as u128) as u64;
    scaled.clamp(min_ticks.min(full_ticks), full_ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模擬 parser：含 "BAD" 的行讓整段回傳 Rejected、含 "LOOP" 的行讓整段 trap，
    /// 其餘逐行轉成大寫。
    fn fake_probe<'a>(
        lines: &'a [&'a str],
        calls: &'a mut Vec<(Range<usize>, bool)>,
    ) -> impl FnMut(Range<usize>, bool) -> ProbeOutcome<String> + 'a {
        move |r: Range<usize>, trap: bool| {
            calls.push((r.clone(), trap));
            let seg = &lines[r];
            if seg.iter().any(|l| l.contains("LOOP")) {
                ProbeOutcome::Trapped("epoch".into())
            } else if seg.iter().any(|l| l.contains("BAD")) {
                ProbeOutcome::Rejected("invalid".into())
            } else {
                ProbeOutcome::Accepted(seg.iter().map(|l| l.to_uppercase()).collect())
            }
        }
    }

    #[test]
    fn rejected_single_bad_line_is_isolated_and_order_kept() {
        let lines = ["a", "b", "BAD", "d", "e", "f", "g"];
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Rejected,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        assert_eq!(rep.rejected, vec![(2, RejectReason::PluginError)]);
        let ids = assign_ids(7, 100_000, lines.len(), rep.accepted);
        let got: Vec<(u64, String)> = ids;
        assert_eq!(
            got,
            vec![
                (700_000, "A".to_string()),
                (700_001, "B".to_string()),
                (700_003, "D".to_string()),
                (700_004, "E".to_string()),
                (700_005, "F".to_string()),
                (700_006, "G".to_string()),
            ]
        );
        assert!(!rep.budget_exhausted);
    }

    #[test]
    fn rejected_many_bad_lines_all_found() {
        // 每 20 行一筆壞資料（5%），與 correctness C3 相同比例
        let owned: Vec<String> = (0..1000)
            .map(|i| {
                if i % 20 == 7 {
                    format!("BAD{i}")
                } else {
                    format!("ok{i}")
                }
            })
            .collect();
        let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Rejected,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        let bad: Vec<usize> = rep.rejected.iter().map(|(i, _)| *i).collect();
        assert_eq!(bad, (0..1000).filter(|i| i % 20 == 7).collect::<Vec<_>>());
        assert_eq!(rep.salvaged(), 950);
        // id 唯一且嚴格遞增（= 輸出順序與輸入順序一致）
        let ids = assign_ids(3, 100_000, lines.len(), rep.accepted);
        assert!(ids.windows(2).all(|w| w[0].0 < w[1].0));
        // 呼叫次數 ≈ 2k·log2(n/k)，遠小於逐筆重試的 1000 次
        assert!(rep.calls < 1000, "calls={}", rep.calls);
        assert_eq!(rep.trap_calls, 0);
    }

    #[test]
    fn all_lines_bad_rejects_everything() {
        let lines = ["BAD1", "BAD2", "BAD3", "BAD4", "BAD5"];
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Rejected,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        assert_eq!(rep.rejected.len(), 5);
        assert_eq!(rep.salvaged(), 0);
        // 最壞情況 2n-2 次探測（不含最初整批）
        assert!(rep.calls <= 2 * lines.len() as u32 - 2);
    }

    #[test]
    fn single_line_batch_is_rejected_without_probe() {
        let lines = ["BAD"];
        let mut calls = Vec::new();
        let rep = isolate(
            1,
            FailureKind::Rejected,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        assert_eq!(rep.rejected, vec![(0, RejectReason::PluginError)]);
        assert_eq!(rep.calls, 0);
        drop(rep);
        assert!(calls.is_empty());
    }

    #[test]
    fn poison_record_isolated_with_trap_mode_flag() {
        let owned: Vec<String> = (0..64)
            .map(|i| {
                if i == 41 {
                    "LOOP".into()
                } else {
                    format!("x{i}")
                }
            })
            .collect();
        let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Trapped,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        assert_eq!(rep.rejected, vec![(41, RejectReason::Poison)]);
        assert_eq!(rep.salvaged(), 63);
        // 64 筆：每層 2 次、共 6 層 = 12 次，全部在 trap 模式
        assert_eq!(rep.trap_calls, 12);
        assert!(!rep.budget_exhausted);
        assert!(calls.iter().all(|(_, trap)| *trap));
    }

    #[test]
    fn trap_budget_limits_calls_and_rejects_remaining_range() {
        let owned: Vec<String> = (0..1024)
            .map(|i| {
                if i == 500 {
                    "LOOP".into()
                } else {
                    format!("x{i}")
                }
            })
            .collect();
        let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let limits = IsolationLimits {
            max_trap_calls: 6,
            max_trap_depth: 16,
            ..DEFAULT_LIMITS
        };
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Trapped,
            limits,
            fake_probe(&lines, &mut calls),
        );
        assert!(rep.budget_exhausted);
        assert!(rep.trap_calls <= 6);
        // 3 層後剩 1024/8 = 128 筆的區段仍含 poison，整段拒絕
        let exhausted = rep
            .rejected
            .iter()
            .filter(|(_, r)| *r == RejectReason::BudgetExhausted)
            .count();
        assert_eq!(exhausted, 128);
        assert!(rep.rejected.iter().any(|(i, _)| *i == 500));
        assert_eq!(rep.salvaged(), 1024 - 128);
    }

    #[test]
    fn trap_depth_limit_respected() {
        let owned: Vec<String> = (0..256)
            .map(|i| {
                if i == 3 {
                    "LOOP".into()
                } else {
                    format!("x{i}")
                }
            })
            .collect();
        let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let limits = IsolationLimits {
            max_trap_calls: 1000,
            max_trap_depth: 2,
            ..DEFAULT_LIMITS
        };
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Trapped,
            limits,
            fake_probe(&lines, &mut calls),
        );
        assert!(rep.budget_exhausted);
        assert_eq!(rep.salvaged(), 256 - 64);
    }

    #[test]
    fn mixed_malformed_inside_trap_range_switches_to_rejected_mode() {
        let lines = ["a", "BAD", "c", "d", "LOOP", "f", "g", "h"];
        let mut calls = Vec::new();
        let rep = isolate(
            lines.len(),
            FailureKind::Trapped,
            DEFAULT_LIMITS,
            fake_probe(&lines, &mut calls),
        );
        assert_eq!(
            rep.rejected,
            vec![(1, RejectReason::PluginError), (4, RejectReason::Poison)]
        );
        assert_eq!(rep.salvaged(), 6);
    }

    #[test]
    fn assign_ids_handles_non_one_to_one_parser() {
        // 第二段輸出比輸入多一筆：多出的那筆編到 batch_len 之後，不與其他 id 衝突
        let accepted = vec![(0..2, vec!["a", "b"]), (3..4, vec!["d", "d2"])];
        let ids: Vec<u64> = assign_ids(1, 100, 4, accepted)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![100, 101, 103, 104]);
    }

    #[test]
    fn probe_deadline_scales_and_clamps() {
        // 完整 deadline 3000 ticks（30 s）、批次 1000 筆
        assert_eq!(probe_deadline_ticks(3000, 1000, 500, 10), 3000);
        assert_eq!(probe_deadline_ticks(3000, 1000, 250, 10), 1500);
        assert_eq!(probe_deadline_ticks(3000, 1000, 1, 10), 10);
        // 下限不得超過完整 deadline
        assert_eq!(probe_deadline_ticks(5, 1000, 1, 10), 5);
    }

    #[test]
    fn gave_up_rejects_the_range_without_splitting() {
        let mut calls = 0;
        let rep: IsolationReport<u8> = isolate(8, FailureKind::Rejected, DEFAULT_LIMITS, |_, _| {
            calls += 1;
            ProbeOutcome::GaveUp("endpoint down".into())
        });
        assert_eq!(calls, 2, "左右半段各探測一次後就停止");
        assert_eq!(rep.rejected.len(), 8);
        assert!(
            rep.rejected
                .iter()
                .all(|(_, r)| *r == RejectReason::DeliveryFailed)
        );
    }

    #[test]
    fn rejected_budget_limits_calls() {
        let limits = IsolationLimits {
            max_rejected_calls: 4,
            ..DEFAULT_LIMITS
        };
        let mut calls = 0;
        let rep: IsolationReport<u8> = isolate(1024, FailureKind::Rejected, limits, |_, _| {
            calls += 1;
            ProbeOutcome::Rejected("bad".into())
        });
        assert!(calls <= 4, "calls={calls}");
        assert_eq!(rep.rejected.len(), 1024, "沒有定位到的區段也必須全部回報");
        assert!(rep.budget_exhausted);
    }
}
