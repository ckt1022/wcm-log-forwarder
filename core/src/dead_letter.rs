// 所有無法繼續處理的日誌都寫到這裡，確保資料不會只留在記憶體裡消失。
// 路徑預設為工作目錄下的 error.txt，可用環境變數 WCM_ERROR_FILE 覆寫。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::isolation::RejectReason;

static RECORDS: AtomicU64 = AtomicU64::new(0);
// 多個 transport worker 可能同時寫入：一次呼叫的標頭與內容必須連續
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn open() -> std::io::Result<File> {
    let path = std::env::var("WCM_ERROR_FILE").unwrap_or_else(|_| "error.txt".to_string());
    OpenOptions::new().create(true).append(true).open(path)
}

/// 把一段標頭和多筆資料寫入 dead-letter 檔案。
pub fn write<I>(header: &str, records: I)
where
    I: IntoIterator<Item = String>,
{
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut file = match open() {
        Ok(f) => f,
        Err(e) => {
            // 檔案也寫不進去時，最後的保存手段是 stderr
            eprintln!("[dead-letter] cannot open file: {e}; dumping to stderr: {header}");
            for r in records {
                eprintln!("[dead-letter] {r}");
                RECORDS.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    };
    if let Err(e) = writeln!(file, "{header}") {
        eprintln!("[dead-letter] write failed: {e}");
    }
    for r in records {
        if let Err(e) = writeln!(file, "{r}") {
            eprintln!("[dead-letter] write failed: {e}; record: {r}");
        }
        RECORDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// 寫入一批隔離後被拒絕的資料；`record(i)` 取得原批次第 i 筆的文字。
pub fn write_rejected(
    stage: &str,
    seq: u64,
    batch_len: usize,
    rejected: &[(usize, RejectReason)],
    mut record: impl FnMut(usize) -> String,
) {
    if rejected.is_empty() {
        return;
    }
    let shown: Vec<String> = rejected
        .iter()
        .take(50)
        .map(|(i, r)| format!("{}:{}", i, r.as_str()))
        .collect();
    let header = format!(
        "[{stage}-isolate] batch={seq} 拒絕 {}/{batch_len} 筆（index:原因{}）{}",
        rejected.len(),
        if rejected.len() > 50 {
            "，僅列前 50"
        } else {
            ""
        },
        shown.join(","),
    );
    write(&header, rejected.iter().map(|(i, _)| record(*i)));
}

/// 目前累計寫入的筆數。
pub fn total_records() -> u64 {
    RECORDS.load(Ordering::Relaxed)
}
