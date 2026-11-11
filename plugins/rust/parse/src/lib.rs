wit_bindgen::generate!({
    path: "../../../core/wit",
    world: "parser-plugin",
    generate_all,
});

use std::sync::atomic::{AtomicU64, Ordering};

use crate::local::log_process::pipeline_process::LogLevel;
use serde_json::Value;
use wasi::clocks::monotonic_clock;

static LAST_EXEC_NS: AtomicU64 = AtomicU64::new(0);

/// 將 JSON 值轉為字串：字串直接取值，其餘型別輸出 compact JSON
/// （對應 C 版的 cJSON_PrintUnformatted）
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 映射字串 Log Level 到系統定義的數值
fn map_level(level_str: Option<&str>) -> LogLevel {
    match level_str {
        Some(s) if s.eq_ignore_ascii_case("debug") => LogLevel::Debug,
        Some(s) if s.eq_ignore_ascii_case("info") => LogLevel::Info,
        Some(s) if s.eq_ignore_ascii_case("warn") => LogLevel::Warn,
        Some(s) if s.eq_ignore_ascii_case("error") => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// 數值型 level 欄位的映射（syslog 七級）
fn level_from_number(n: u64) -> LogLevel {
    match n {
        0 => LogLevel::Debug,
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        3 => LogLevel::Error,
        4 => LogLevel::Crit,
        5 => LogLevel::Alert,
        6 => LogLevel::Emerg,
        _ => LogLevel::Info,
    }
}

/// 根據 log level 回傳 route 標籤（同 Go plugin 的 routeTag 邏輯）
fn route_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error | LogLevel::Crit | LogLevel::Alert | LogLevel::Emerg => "A",
        LogLevel::Warn => "A",
        _ => "A",
    }
}

/// 輔助函數：將 JSON 物件中的欄位轉換為 Tags 列表
/// 包含：1. 根目錄下的非保留欄位 2. "att" 物件內的所有欄位
fn collect_all_tags(root: &Value) -> Vec<(String, String)> {
    // 第一個 tag：lang=Rust（標識此批次由 Rust 插件解析）
    let mut tags = vec![("lang".to_string(), "Rust".to_string())];

    if let Value::Object(map) = root {
        // 填充根目錄下的非保留欄位
        for (key, val) in map {
            if matches!(key.as_str(), "ts" | "level" | "msg" | "att") {
                continue;
            }
            tags.push((key.clone(), value_to_string(val)));
        }

        // 填充 att 內的欄位
        if let Some(Value::Object(att)) = map.get("att") {
            for (key, val) in att {
                tags.push((key.clone(), value_to_string(val)));
            }
        }
    }

    tags
}

struct Component;

impl Guest for Component {
    fn parse(raw_data: Vec<String>) -> Result<Vec<ParsedEntry>, ParseError> {
        let start_ns = monotonic_clock::now();

        let mut entries = Vec::with_capacity(raw_data.len());

        for raw in &raw_data {
            let root: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(_) => {
                    LAST_EXEC_NS.store(
                        monotonic_clock::now().saturating_sub(start_ns),
                        Ordering::Relaxed,
                    );
                    return Err(ParseError::InvalidFormat("JSON Parse Error".to_string()));
                }
            };

            // 1. 解析 ts (timestamp)
            let timestamp = root
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            // 2. 解析 level (處理字串轉數值)
            let level = match root.get("level") {
                Some(Value::Number(n)) => level_from_number(n.as_u64().unwrap_or(1)),
                lv => map_level(lv.and_then(Value::as_str)),
            };

            // 3. 解析 msg (message)
            let message = root
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            // 4. 自動收集所有標籤 (含根目錄非保留欄位與 att 物件)
            let tags = collect_all_tags(&root);

            // 5. 設定 route 標籤
            let targettag = route_tag(level).to_string();

            entries.push(ParsedEntry {
                timestamp,
                level,
                message,
                tags,
                targettag,
            });
        }

        LAST_EXEC_NS.store(
            monotonic_clock::now().saturating_sub(start_ns),
            Ordering::Relaxed,
        );

        Ok(entries)
    }

    fn report_usage() -> u64 {
        LAST_EXEC_NS.load(Ordering::Relaxed)
    }
}

export!(Component);
