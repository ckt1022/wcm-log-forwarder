// latency_filter — Fluent Bit WASM filter（延遲對比實驗，Rust 版）
//
// 功能與 ../latency_filter.lua 完全相同，對齊 WCM forwarder 的 Rust plugin pipeline：
//   parse : ts 原字串搬到 timestamp、level 字串/數值映射、msg → message、
//           根層非保留欄位與 att.* 攤平
//   filter: 只保留 level >= warn（丟棄 debug / info）
//   format: json-flat 欄位集合 id/timestamp/level/message/lang/<tags>
//
// ABI 同 wasm_filter/wasm_file 的 crash_* 系列（fluent-bit filter_wasm）：
//   filter_func(tag, tag_len, time_sec, time_nsec, record_json, record_len) -> *const u8
//   回傳 NULL = 丟棄該筆；回傳 NUL 結尾的 JSON 字串指標 = 取代 record。
//
// 關鍵約束：timestamp 必須是 gen 原始 ts 字串原封不動，
// HTTP 接收端（common/tools/httpsink）靠這個欄位計算延遲。

use std::cell::{Cell, RefCell};

use serde_json::{Map, Value};

// index = rank：debug=0 info=1 warn=2 error=3 crit=4 alert=5 emerg=6（同 WIT LogLevel）
const LEVEL_NAMES: [&str; 7] = ["debug", "info", "warn", "error", "crit", "alert", "emerg"];
const MIN_KEEP_RANK: usize = 2; // warn（同 filter plugin 的 MIN_KEEP_LEVEL）

thread_local! {
    // 輸出緩衝：回傳的指標必須在函式返回後仍有效，故存放於 thread-local
    static OUT_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    // 對應 forwarder host 端遞增的 entry id（僅為欄位對齊，值本身不影響量測）
    static ID_COUNTER: Cell<u64> = const { Cell::new(0) };
}

/// 對應 parse plugin 的 map_level / level_from_number：
/// 字串不分大小寫映射；數值 0~6 直接取；缺失或未知 → info
fn level_rank(v: Option<&Value>) -> usize {
    match v {
        Some(Value::String(s)) => {
            let ls = s.to_ascii_lowercase();
            LEVEL_NAMES.iter().position(|n| *n == ls).unwrap_or(1)
        }
        Some(Value::Number(n)) => match n.as_u64() {
            Some(x) if x <= 6 => x as usize,
            _ => 1,
        },
        _ => 1,
    }
}

/// 對應 parse plugin 的 value_to_string：字串直接取值，其餘輸出 compact JSON
fn value_to_string(v: &Value) -> Value {
    match v {
        Value::String(_) => v.clone(),
        other => Value::String(other.to_string()),
    }
}

/// 回傳 None = 丟棄（level 不足或 JSON 解析失敗）；Some = 取代後的 record JSON
fn process(data: &[u8]) -> Option<Vec<u8>> {
    let root: Value = serde_json::from_slice(data).ok()?;
    let map = root.as_object()?;

    let rank = level_rank(map.get("level"));
    if rank < MIN_KEEP_RANK {
        return None; // drop（debug / info）
    }

    let id = ID_COUNTER.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });

    let mut out = Map::new();
    out.insert("id".into(), Value::from(id));
    out.insert(
        "timestamp".into(),
        Value::String(
            map.get("ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    );
    out.insert("level".into(), Value::String(LEVEL_NAMES[rank].to_string()));
    out.insert(
        "message".into(),
        Value::String(
            map.get("msg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    );
    out.insert("lang".into(), Value::String("Rust".to_string()));

    // 根層非保留欄位（對應 collect_all_tags 的第一段）
    for (k, v) in map {
        if matches!(k.as_str(), "ts" | "level" | "msg" | "att") {
            continue;
        }
        out.insert(k.clone(), value_to_string(v));
    }

    // att 物件內欄位攤平（對應 collect_all_tags 的第二段）
    if let Some(Value::Object(att)) = map.get("att") {
        for (k, v) in att {
            out.insert(k.clone(), value_to_string(v));
        }
    }

    serde_json::to_vec(&Value::Object(out)).ok()
}

/// # Safety
/// 由 fluent-bit filter_wasm 呼叫：obj 指向 record 的 JSON 序列化字串。
#[no_mangle]
pub unsafe extern "C" fn filter_func(
    _tag: *const u8,
    _tag_len: i32,
    _time_sec: u32,
    _time_nsec: u32,
    obj: *const u8,
    obj_len: i32,
) -> *const u8 {
    if obj.is_null() || obj_len <= 0 {
        return std::ptr::null();
    }
    let data = std::slice::from_raw_parts(obj, obj_len as usize);

    match process(data) {
        Some(json) => OUT_BUF.with(|b| {
            let mut buf = b.borrow_mut();
            *buf = json;
            buf.push(0); // host 以 NUL 結尾字串讀取
            buf.as_ptr()
        }),
        None => std::ptr::null(),
    }
}
