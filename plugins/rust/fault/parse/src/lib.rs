// fault_parse：與 rust/parse 相同，另外依 msg 內的 marker 在處理該行時觸發故障（marker 見 ../probe.rs）。
// FILE／ENV／NET 的探測結果同時寫到 stderr 與回傳的 message。

wit_bindgen::generate!({
    path: "../../../../core/wit",
    world: "parser-plugin",
    generate_all,
});

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use crate::local::log_process::pipeline_process::LogLevel;
use wasi::clocks::monotonic_clock;

static LAST: AtomicU64 = AtomicU64::new(0);

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn map_level(s: Option<&str>) -> LogLevel {
    match s {
        Some(s) if s.eq_ignore_ascii_case("debug") => LogLevel::Debug,
        Some(s) if s.eq_ignore_ascii_case("warn") => LogLevel::Warn,
        Some(s) if s.eq_ignore_ascii_case("error") => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

fn trigger(msg: &str) -> ParsedEntry {
    if msg.contains("FAULT_LOOP") {
        // 無窮迴圈：靠 host epoch deadline 以 trap 中止
        let mut x: u64 = 0;
        loop {
            x = x.wrapping_add(1);
            black_box(x);
        }
    } else if msg.contains("FAULT_MEM") {
        // 無上限成長：每次 32 MiB，直到 ResourceLimiter 拒絕 grow → 配置失敗 → trap
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut c = vec![0u8; 32 * 1024 * 1024];
            c[0] = 1;
            black_box(&c[0]);
            chunks.push(c);
            black_box(chunks.len());
        }
    } else if msg.contains("FAULT_PANIC") {
        panic!("fault_parse: FAULT_PANIC marker");
    } else if msg.contains("FAULT_OOB") {
        let v: Vec<u8> = vec![1, 2, 3];
        let i = black_box(1_000_000usize);
        black_box(v[i]); // panic → trap
        unreachable!()
    } else if msg.contains("FAULT_ENV") {
        // 嘗試讀環境變數；host 未注入任何值，應為空字串
        let leaked: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
        let bytes: usize = leaked.iter().map(|s| s.len()).sum();
        let message = format!("FAULT_ENV result: {} vars visible ({} bytes): {}", leaked.len(), bytes, leaked.join(","));
        eprintln!("[fault_parse] PROBE env vars={} bytes={} outcome={}", leaked.len(), bytes,
                  if leaked.is_empty() { "DENIED(empty)" } else { "LEAK" });
        ParsedEntry {
            timestamp: String::new(),
            level: LogLevel::Warn,
            message,
            tags: vec![("lang".into(), "Rust".into()), ("probe".into(), "env".into())],
            targettag: "A".into(),
        }
    } else if msg.contains("FAULT_NET") {
        // 嘗試對 127.0.0.1:8080（實驗的 HTTP sink，確實在 listen）建立 TCP 連線
        let target = "127.0.0.1:8080";
        let r = std::net::TcpStream::connect(target);
        let message = match &r {
            Ok(s) => format!("FAULT_NET LEAK: connected to {target} (local={:?})", s.local_addr().ok()),
            Err(e) => format!("FAULT_NET blocked: connect {target} failed: kind={:?} err={e}", e.kind()),
        };
        eprintln!("[fault_parse] PROBE net target={target} outcome={} detail={message}",
                  if r.is_ok() { "LEAK" } else { "DENIED" });
        drop(r);
        ParsedEntry {
            timestamp: String::new(),
            level: LogLevel::Warn,
            message,
            tags: vec![("lang".into(), "Rust".into()), ("probe".into(), "net".into())],
            targettag: "A".into(),
        }
    } else {
        // FAULT_FILE：嘗試讀 host 檔案；host 無 preopen，應失敗
        let r = std::fs::read_to_string("/etc/passwd");
        let msg = match &r {
            Ok(s) => format!("FAULT_FILE LEAK: read {} bytes of /etc/passwd", s.len()),
            Err(e) => format!("FAULT_FILE blocked: kind={:?} err={e}", e.kind()),
        };
        eprintln!("[fault_parse] PROBE file path=/etc/passwd outcome={} detail={msg}",
                  if r.is_ok() { "LEAK" } else { "DENIED" });
        ParsedEntry {
            timestamp: String::new(),
            level: LogLevel::Warn,
            message: msg,
            tags: vec![("lang".into(), "Rust".into()), ("probe".into(), "file".into())],
            targettag: "A".into(),
        }
    }
}

struct Component;

impl Guest for Component {
    fn parse(raw: Vec<String>) -> Result<Vec<ParsedEntry>, ParseError> {
        let start = monotonic_clock::now();
        let mut out = Vec::with_capacity(raw.len());
        for line in &raw {
            let root: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    LAST.store(monotonic_clock::now().saturating_sub(start), Ordering::Relaxed);
                    return Err(ParseError::InvalidFormat("JSON Parse Error".into()));
                }
            };
            let message = root.get("msg").and_then(Value::as_str).unwrap_or("").to_string();
            if message.contains("FAULT_ERR") {
                return Err(ParseError::InvalidFormat("injected fault".into()));
            }
            if message.contains("FAULT_") {
                out.push(trigger(&message));
                continue;
            }
            let timestamp = root.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
            let level = map_level(root.get("level").and_then(Value::as_str));
            let mut tags = vec![("lang".to_string(), "Rust".to_string())];
            if let Value::Object(m) = &root {
                for (k, v) in m {
                    if matches!(k.as_str(), "ts" | "level" | "msg" | "att") {
                        continue;
                    }
                    tags.push((k.clone(), value_to_string(v)));
                }
                if let Some(Value::Object(att)) = m.get("att") {
                    for (k, v) in att {
                        tags.push((k.clone(), value_to_string(v)));
                    }
                }
            }
            out.push(ParsedEntry { timestamp, level, message, tags, targettag: "A".into() });
        }
        LAST.store(monotonic_clock::now().saturating_sub(start), Ordering::Relaxed);
        Ok(out)
    }

    fn report_usage() -> u64 {
        LAST.load(Ordering::Relaxed)
    }
}

export!(Component);
