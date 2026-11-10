// 輸入來源（TCP／tail）與記憶體背壓。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use crate::metrics::Metrics;
use crate::{shutdown, spawn_named};

// ── Memory guard ─────────────────────────────────────────────────────────────
//
// 各元件各自守自己的配額，但 process 總 RSS 沒有機制保證低於容器 cgroup 上限；
// 超標時被 OOM kill，對上游表現為 connection reset（掉資料），而不是背壓。
//
// 做法：背景執行緒每 100 ms 讀 cgroup v2 的 memory.current／memory.max，
// 超過高水位就停止讀取輸入（不斷線、不丟資料），kernel buffer 滿了以後上游被
// TCP 流控卡住；降回低水位才恢復。讀不到 cgroup 檔案時自動停用。

static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);
static MEM_GUARD_INIT: OnceLock<()> = OnceLock::new();

// 暫停輸入後，管線內的資料仍會經 parse／format 膨脹（2～4 倍），需要足夠餘裕；
// 可用環境變數 MEM_GUARD_HIGH／MEM_GUARD_LOW 覆寫（0～1 小數）。
const MEM_GUARD_HIGH_DEFAULT: f64 = 0.70;
const MEM_GUARD_LOW_DEFAULT: f64 = 0.55;
const MEM_GUARD_POLL: Duration = Duration::from_millis(100);

/// 輸入執行緒檢查 shutdown 旗標的間隔。
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

fn guard_level(env_key: &str, default: f64) -> f64 {
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0 && *v < 1.0)
        .unwrap_or(default)
}

fn read_cgroup_mem() -> Option<(u64, u64)> {
    let cur: u64 = std::fs::read_to_string("/sys/fs/cgroup/memory.current")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // memory.max 內容為 "max" 代表無上限 → parse 失敗 → guard 停用
    let max: u64 = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((cur, max))
}

fn init_mem_guard() {
    MEM_GUARD_INIT.get_or_init(|| {
        let Some((cur, max)) = read_cgroup_mem() else {
            eprintln!("[mem-guard] disabled: cgroup memory.current/memory.max 不可讀或無上限");
            return;
        };
        let high = guard_level("MEM_GUARD_HIGH", MEM_GUARD_HIGH_DEFAULT);
        let low = guard_level("MEM_GUARD_LOW", MEM_GUARD_LOW_DEFAULT).min(high - 0.05);
        eprintln!(
            "[mem-guard] enabled: limit={}MB high={:.0}% low={:.0}% current={}MB",
            max / 1048576,
            high * 100.0,
            low * 100.0,
            cur / 1048576
        );
        spawn_named("mem-guard", move || {
            loop {
                // 暫停期間把 allocator 留存的空閒 heap 還給 OS：
                // 不 trim 的話 memory.current 停在高點，輸入會永久暫停。
                if INPUT_PAUSED.load(Ordering::Relaxed) {
                    // SAFETY: malloc_trim 沒有前置條件
                    unsafe {
                        libc::malloc_trim(0);
                    }
                }
                if let Some((cur, max)) = read_cgroup_mem() {
                    let ratio = cur as f64 / max as f64;
                    let paused = INPUT_PAUSED.load(Ordering::Relaxed);
                    if !paused && ratio >= high {
                        INPUT_PAUSED.store(true, Ordering::Relaxed);
                        eprintln!(
                            "[mem-guard] PAUSE input: {}MB ({:.0}%)",
                            cur / 1048576,
                            ratio * 100.0
                        );
                    } else if paused && ratio <= low {
                        INPUT_PAUSED.store(false, Ordering::Relaxed);
                        eprintln!(
                            "[mem-guard] RESUME input: {}MB ({:.0}%)",
                            cur / 1048576,
                            ratio * 100.0
                        );
                    }
                }
                thread::sleep(MEM_GUARD_POLL);
            }
        });
    });
}

fn input_paused() -> bool {
    INPUT_PAUSED.load(Ordering::Relaxed)
}

// ── Readers ──────────────────────────────────────────────────────────────────

/// 監聽 TCP 連接埠，把每條連線的每一行送進 channel。
/// 收到 shutdown 後停止接受與讀取；所有 sender 釋放後管線開始收尾。
pub fn spawn_tcp_reader(host: String, port: u16, tx: SyncSender<String>, metrics: Arc<Metrics>) {
    init_mem_guard();
    spawn_named("tcp-input", move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("[tcp-input] tokio runtime build failed");
        rt.block_on(async move {
            let addr = format!("{host}:{port}");
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .unwrap_or_else(|e| panic!("[tcp-input] cannot bind {addr}: {e}"));
            eprintln!("[tcp-input] listening on {addr}");
            let mut connections = Vec::new();
            while !shutdown::requested() {
                let Ok(accepted) = tokio::time::timeout(SHUTDOWN_POLL, listener.accept()).await
                else {
                    continue;
                };
                match accepted {
                    Ok((socket, peer)) => {
                        eprintln!("[tcp-input] accepted {peer}");
                        connections.retain(|h: &tokio::task::JoinHandle<()>| !h.is_finished());
                        connections.push(tokio::spawn(read_connection(
                            socket,
                            peer,
                            tx.clone(),
                            Arc::clone(&metrics),
                        )));
                    }
                    Err(e) => eprintln!("[tcp-input] accept error: {e}"),
                }
            }
            for handle in connections {
                let _ = handle.await;
            }
        });
    });
}

async fn read_connection(
    socket: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    tx: SyncSender<String>,
    metrics: Arc<Metrics>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(socket).lines();
    while !shutdown::requested() {
        // mem-guard 高水位：暫停讀取（不斷線），讓 TCP 流控回壓上游
        if input_paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        // next_line 可安全取消：逾時只代表暫時沒資料，已讀的部分保留在 buffer
        match tokio::time::timeout(SHUTDOWN_POLL, lines.next_line()).await {
            Err(_) => continue,
            Ok(Ok(Some(line))) => {
                if tx.send(line).is_err() {
                    break;
                }
                metrics.input_records.add(1);
            }
            Ok(_) => break,
        }
    }
    eprintln!("[tcp-input] {peer} disconnected");
}

/// 類似 `tail -f`：啟動時跳到檔尾，之後持續讀取新增的行。
pub fn spawn_tail_reader(path: String, tx: SyncSender<String>, metrics: Arc<Metrics>) {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    init_mem_guard();
    spawn_named("tail-input", move || {
        let mut file = loop {
            if shutdown::requested() {
                return;
            }
            match File::open(&path) {
                Ok(f) => break f,
                Err(e) => {
                    eprintln!("[tail-input] waiting for '{path}': {e}");
                    thread::sleep(Duration::from_secs(1));
                }
            }
        };
        let _ = file.seek(SeekFrom::End(0));
        eprintln!("[tail-input] tailing '{path}'");

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        while !shutdown::requested() {
            // mem-guard 高水位：暫停讀取，檔案 offset 停在原地不丟資料
            if input_paused() {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => thread::sleep(Duration::from_millis(50)),
                Ok(_) => {
                    if line.ends_with('\n') {
                        line.pop();
                        if line.ends_with('\r') {
                            line.pop();
                        }
                    }
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                    metrics.input_records.add(1);
                }
                Err(e) => {
                    eprintln!("[tail-input] read error: {e}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}
