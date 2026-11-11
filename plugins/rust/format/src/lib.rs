// lib.rs — compile-time selectable log encoding
//
// All encodings share the same wire framing:
//   [4-byte LE length][entry bytes] repeated per entry
// This lets the host decode entries uniformly regardless of encoding.
//
// Select encoding via cargo feature (only one):
//   (none) / --features json-flat   flat JSON object per entry (default)
//   --features json-nested          JSON object with tags under "attributes"
//   --features syslog               RFC 5424 syslog line (includes trailing \n)
//   --features protobuf             protobuf binary per entry

wit_bindgen::generate!({
    path: "../../../core/wit",
    world: "format-plugin",
    generate_all,
});

use std::sync::atomic::{AtomicU64, Ordering};

use wasi::clocks::monotonic_clock;

#[cfg(any(
    all(feature = "json-flat", any(feature = "json-nested", feature = "syslog", feature = "protobuf")),
    all(feature = "json-nested", any(feature = "syslog", feature = "protobuf")),
    all(feature = "syslog", feature = "protobuf"),
))]
compile_error!("select at most one encoding feature: json-flat / json-nested / syslog / protobuf");

static LAST_EXEC_NS: AtomicU64 = AtomicU64::new(0);

// ─── common helpers ───────────────────────────────────────────────────────────

#[cfg(not(feature = "protobuf"))]
use crate::local::log_process::pipeline_process::LogLevel;

#[cfg(any(
    feature = "json-flat",
    feature = "json-nested",
    not(any(feature = "syslog", feature = "protobuf"))
))]
fn level_name(level: LogLevel) -> &'static str {
    const NAMES: [&str; 7] = ["debug", "info", "warn", "error", "crit", "alert", "emerg"];
    NAMES
        .get(level as u8 as usize)
        .copied()
        .unwrap_or("unknown")
}

/// Emit [4-byte LE length][data] into `out`.
fn write_frame(out: &mut Vec<u8>, entry: &[u8]) {
    out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
    out.extend_from_slice(entry);
}

// ─── JSON helpers ─────────────────────────────────────────────────────────────

#[cfg(any(
    feature = "json-flat",
    feature = "json-nested",
    not(any(feature = "syslog", feature = "protobuf"))
))]
fn json_escape(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &c in s.as_bytes() {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

// ─── protobuf helpers ────────────────────────────────────────────────────────

#[cfg(feature = "protobuf")]
fn pb_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

#[cfg(feature = "protobuf")]
fn pb_tag(out: &mut Vec<u8>, field: u32, wtype: u8) {
    pb_varint(out, ((field as u64) << 3) | wtype as u64);
}

#[cfg(feature = "protobuf")]
fn pb_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    if v == 0 {
        return;
    }
    pb_tag(out, field, 0);
    pb_varint(out, v);
}

#[cfg(feature = "protobuf")]
fn pb_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    pb_tag(out, field, 2);
    pb_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

// ─── per-entry encoders ───────────────────────────────────────────────────────
// Each encoder builds one entry into a local Vec, then calls write_frame().

// ── 1. JSON flat ──────────────────────────────────────────────────────────────
// 明選 json-flat，或四個 feature 都沒選時的預設（對應 C++ 版的行為）
#[cfg(any(
    feature = "json-flat",
    not(any(feature = "json-nested", feature = "syslog", feature = "protobuf"))
))]
fn encode_entry(out: &mut Vec<u8>, e: &LogEntry) {
    let mut entry = Vec::new();
    entry.extend_from_slice(b"{\"id\":");
    entry.extend_from_slice(e.id.to_string().as_bytes());
    entry.extend_from_slice(b",\"timestamp\":");
    json_escape(&mut entry, &e.timestamp);
    entry.extend_from_slice(b",\"level\":\"");
    entry.extend_from_slice(level_name(e.level).as_bytes());
    entry.extend_from_slice(b"\",\"message\":");
    json_escape(&mut entry, &e.message);
    for (key, val) in &e.tags {
        entry.push(b',');
        json_escape(&mut entry, key);
        entry.push(b':');
        json_escape(&mut entry, val);
    }
    entry.push(b'}');
    write_frame(out, &entry);
}

// ── 2. JSON nested ────────────────────────────────────────────────────────────
#[cfg(feature = "json-nested")]
fn encode_entry(out: &mut Vec<u8>, e: &LogEntry) {
    let mut entry = Vec::new();
    entry.extend_from_slice(b"{\"id\":");
    entry.extend_from_slice(e.id.to_string().as_bytes());
    entry.extend_from_slice(b",\"timestamp\":");
    json_escape(&mut entry, &e.timestamp);
    entry.extend_from_slice(b",\"level\":\"");
    entry.extend_from_slice(level_name(e.level).as_bytes());
    entry.extend_from_slice(b"\",\"message\":");
    json_escape(&mut entry, &e.message);
    entry.extend_from_slice(b",\"attributes\":{");
    for (i, (key, val)) in e.tags.iter().enumerate() {
        if i > 0 {
            entry.push(b',');
        }
        json_escape(&mut entry, key);
        entry.push(b':');
        json_escape(&mut entry, val);
    }
    entry.extend_from_slice(b"}}");
    write_frame(out, &entry);
}

// ── 3. RFC 5424 syslog ────────────────────────────────────────────────────────
#[cfg(feature = "syslog")]
fn level_to_pri(level: LogLevel) -> u32 {
    const SEV: [u32; 7] = [7, 6, 4, 3, 2, 1, 0];
    1 * 8 + SEV.get(level as u8 as usize).copied().unwrap_or(6)
}

#[cfg(feature = "syslog")]
fn sd_escape(out: &mut Vec<u8>, s: &str) {
    for &c in s.as_bytes() {
        if c == b'"' || c == b'\\' || c == b']' {
            out.push(b'\\');
        }
        out.push(c);
    }
}

#[cfg(feature = "syslog")]
fn encode_entry(out: &mut Vec<u8>, e: &LogEntry) {
    let mut entry = Vec::new();

    entry.extend_from_slice(format!("<{}>1 ", level_to_pri(e.level)).as_bytes());

    if e.timestamp.is_empty() {
        entry.push(b'-');
    } else {
        entry.extend_from_slice(e.timestamp.as_bytes());
    }
    entry.push(b' ');

    let mut hostname = "-";
    let mut appname = "-";
    for (key, val) in &e.tags {
        match key.as_str() {
            "host" | "hostname" => hostname = val,
            "app" | "appname" => appname = val,
            _ => {}
        }
    }
    entry.extend_from_slice(hostname.as_bytes());
    entry.push(b' ');
    entry.extend_from_slice(appname.as_bytes());
    entry.extend_from_slice(b" - - ");

    let mut has_sd = false;
    for (key, val) in &e.tags {
        if matches!(key.as_str(), "host" | "hostname" | "app" | "appname") {
            continue;
        }
        if !has_sd {
            entry.extend_from_slice(b"[log@12345");
            has_sd = true;
        }
        entry.push(b' ');
        entry.extend_from_slice(key.as_bytes());
        entry.extend_from_slice(b"=\"");
        sd_escape(&mut entry, val);
        entry.push(b'"');
    }
    if has_sd {
        entry.push(b']');
    } else {
        entry.push(b'-');
    }

    entry.push(b' ');
    entry.extend_from_slice(e.message.as_bytes());
    entry.push(b'\n');

    write_frame(out, &entry);
}

// ── 4. Protocol Buffers ───────────────────────────────────────────────────────
//   message Tag      { string key = 1; string value = 2; }
//   message LogEntry {
//     uint64 id = 1;  string timestamp = 2;
//     uint32 level = 3;  string message = 4;
//     repeated Tag tags = 5;
//   }
#[cfg(feature = "protobuf")]
fn encode_entry(out: &mut Vec<u8>, e: &LogEntry) {
    let mut entry = Vec::new();
    pb_uint(&mut entry, 1, e.id);
    pb_bytes(&mut entry, 2, e.timestamp.as_bytes());
    pb_uint(&mut entry, 3, e.level as u64);
    pb_bytes(&mut entry, 4, e.message.as_bytes());
    for (key, val) in &e.tags {
        let mut tag = Vec::new();
        pb_bytes(&mut tag, 1, key.as_bytes());
        pb_bytes(&mut tag, 2, val.as_bytes());
        pb_bytes(&mut entry, 5, &tag);
    }
    write_frame(out, &entry);
}

// ─── exported WIT functions ───────────────────────────────────────────────────

struct Component;

impl Guest for Component {
    fn format(struct_data: Vec<LogEntry>) -> Result<Vec<u8>, PluginError> {
        let start_ns = monotonic_clock::now();

        let mut out = Vec::new();
        for entry in &struct_data {
            encode_entry(&mut out, entry);
        }

        LAST_EXEC_NS.store(
            monotonic_clock::now().saturating_sub(start_ns),
            Ordering::Relaxed,
        );

        Ok(out)
    }

    fn report_usage() -> u64 {
        LAST_EXEC_NS.load(Ordering::Relaxed)
    }
}

export!(Component);
