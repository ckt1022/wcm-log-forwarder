// fault 插件共用：依 message 內的 marker 觸發故障或能力探測，用來驗證 host 的隔離與權限限制。
//
//   FAULT_PANIC  panic → trap
//   FAULT_LOOP   無窮迴圈 → 由 host 的時限中止
//   FAULT_MEM    持續配置記憶體 → 超過 host 的記憶體上限
//   FAULT_OOB    越界存取 → trap
//   FAULT_ERR    回傳 plugin 錯誤
//   FAULT_FILE   讀 /etc/passwd；host 沒有 preopen，應失敗
//   FAULT_ENV    列出環境變數；host 不注入，應為空
//   FAULT_NET    連線到 127.0.0.1:8080 與 8081；未授權時應被拒絕
//
// 因為用到 std::fs 與 std::net，component 會 import wasi:filesystem 與 wasi:sockets，
// 需要 `capabilities.<stage>: { filesystem: inert, sockets: inert }` 才能載入。
// 能力探測的結果寫到 stderr：
//   [<plugin>] PROBE <kind> target=... outcome=DENIED|LEAK|DENIED(empty)|CONNECTED detail=...

use std::hint::black_box;

/// 觸發 trap 類故障；訊息含 FAULT_ERR 時回傳 true，由呼叫端回傳 plugin 錯誤。
pub fn inject(message: &str) -> bool {
    if message.contains("FAULT_PANIC") {
        panic!("injected FAULT_PANIC");
    }
    if message.contains("FAULT_LOOP") {
        let mut x: u64 = 0;
        loop {
            x = black_box(x.wrapping_add(1));
        }
    }
    if message.contains("FAULT_MEM") {
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut chunk = vec![0u8; 32 * 1024 * 1024];
            chunk[0] = 1;
            chunks.push(black_box(chunk));
        }
    }
    if message.contains("FAULT_OOB") {
        let values = [1u8, 2, 3];
        black_box(values[black_box(1_000_000usize)]);
    }
    message.contains("FAULT_ERR")
}

pub fn probe_file(who: &str) {
    let r = std::fs::read_to_string("/etc/passwd");
    let detail = match &r {
        Ok(s) => format!("read {} bytes of /etc/passwd", s.len()),
        Err(e) => format!("kind={:?} err={e}", e.kind()),
    };
    eprintln!("[{who}] PROBE file path=/etc/passwd outcome={} detail={detail}",
              if r.is_ok() { "LEAK" } else { "DENIED" });
}

pub fn probe_env(who: &str) {
    let vars: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let bytes: usize = vars.iter().map(|s| s.len()).sum();
    eprintln!("[{who}] PROBE env vars={} bytes={} outcome={}", vars.len(), bytes,
              if vars.is_empty() { "DENIED(empty)" } else { "LEAK" });
}

/// raw TCP（wasi:sockets）探測；回傳是否連上。
pub fn probe_tcp(who: &str, target: &str, label: &str) -> bool {
    let r = std::net::TcpStream::connect(target);
    let detail = match &r {
        Ok(s) => format!("connected local={:?}", s.local_addr().ok()),
        Err(e) => format!("kind={:?} err={e}", e.kind()),
    };
    eprintln!("[{who}] PROBE net_tcp target={target} ({label}) outcome={} detail={detail}",
              if r.is_ok() { "CONNECTED" } else { "DENIED" });
    r.is_ok()
}

pub fn probe_net(who: &str) {
    probe_tcp(who, "127.0.0.1:8080", "listed-endpoint");
    probe_tcp(who, "127.0.0.1:8081", "not-listed");
}

/// 依 marker 觸發探測；回傳是否有任何 marker。
// fault_transport 依原始 bytes 比對 marker，不使用這個函式
#[allow(dead_code)]
pub fn probe_marker(who: &str, msg: &str) -> bool {
    if msg.contains("FAULT_NET") {
        probe_net(who);
        true
    } else if msg.contains("FAULT_FILE") {
        probe_file(who);
        true
    } else if msg.contains("FAULT_ENV") {
        probe_env(who);
        true
    } else {
        false
    }
}
