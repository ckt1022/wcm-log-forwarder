// format.c — json-flat format-plugin
//
// 每筆輸出一個 JSON 物件：{"id":..,"timestamp":..,"level":..,"message":..,<tag>:<value>...}，
// 以 [4-byte little-endian 長度][資料] 的 frame 串接，host 依 frame 切回每一筆。
// 輸出與 Rust 版 format 的 json-flat 相同。

#include "format_plugin.h"

#include <stdlib.h>
#include <string.h>
#include <time.h>

static uint64_t g_last_exec_ns = 0;

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

typedef struct {
    uint8_t *data;
    size_t len;
    size_t cap;
} buffer_t;

static void reserve(buffer_t *b, size_t extra) {
    if (b->len + extra <= b->cap) {
        return;
    }
    size_t cap = b->cap ? b->cap : 1024;
    while (cap < b->len + extra) {
        cap *= 2;
    }
    b->data = realloc(b->data, cap);
    b->cap = cap;
}

static void append(buffer_t *b, const void *p, size_t n) {
    reserve(b, n);
    memcpy(b->data + b->len, p, n);
    b->len += n;
}

static void append_str(buffer_t *b, const char *s) {
    append(b, s, strlen(s));
}

static void append_u64(buffer_t *b, uint64_t v) {
    char digits[20];
    size_t n = 0;
    do {
        digits[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v > 0);
    while (n > 0) {
        append(b, &digits[--n], 1);
    }
}

static void append_json_string(buffer_t *b, const format_plugin_string_t *s) {
    static const char HEX[] = "0123456789abcdef";
    append(b, "\"", 1);
    for (size_t i = 0; i < s->len; i++) {
        uint8_t c = s->ptr[i];
        switch (c) {
        case '"': append_str(b, "\\\""); break;
        case '\\': append_str(b, "\\\\"); break;
        case '\n': append_str(b, "\\n"); break;
        case '\r': append_str(b, "\\r"); break;
        case '\t': append_str(b, "\\t"); break;
        default:
            if (c < 0x20) {
                char esc[6] = {'\\', 'u', '0', '0', HEX[c >> 4], HEX[c & 0xf]};
                append(b, esc, sizeof(esc));
            } else {
                append(b, &c, 1);
            }
        }
    }
    append(b, "\"", 1);
}

static const char *level_name(local_log_process_pipeline_process_log_level_t level) {
    static const char *NAMES[] = {"debug", "info", "warn", "error", "crit", "alert", "emerg"};
    return level < 7 ? NAMES[level] : "unknown";
}

static void encode_entry(buffer_t *out, const format_plugin_log_entry_t *e) {
    buffer_t entry = {0};
    append_str(&entry, "{\"id\":");
    append_u64(&entry, e->id);
    append_str(&entry, ",\"timestamp\":");
    append_json_string(&entry, &e->timestamp);
    append_str(&entry, ",\"level\":\"");
    append_str(&entry, level_name(e->level));
    append_str(&entry, "\",\"message\":");
    append_json_string(&entry, &e->message);
    for (size_t i = 0; i < e->tags.len; i++) {
        append(&entry, ",", 1);
        append_json_string(&entry, &e->tags.ptr[i].f0);
        append(&entry, ":", 1);
        append_json_string(&entry, &e->tags.ptr[i].f1);
    }
    append(&entry, "}", 1);

    uint32_t n = (uint32_t)entry.len;
    uint8_t header[4] = {n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >> 24) & 0xff};
    append(out, header, sizeof(header));
    append(out, entry.data, entry.len);
    free(entry.data);
}

// wit-bindgen（0.58.0 至 0.63.0 皆同）產生的 log_entry_free 不會釋放 tags，
// 只用產生的函式釋放會讓每筆日誌的標籤洩漏，因此先自行釋放 tags。
static void free_entries(format_plugin_list_log_entry_t *entries) {
    for (size_t i = 0; i < entries->len; i++) {
        format_plugin_list_tuple2_string_string_free(&entries->ptr[i].tags);
    }
    format_plugin_list_log_entry_free(entries);
}

// 匯出函式擁有傳入的參數，用完必須釋放；否則 instance 重用時記憶體會一直累積。
bool exports_format_plugin_format(format_plugin_list_log_entry_t *entries, format_plugin_list_u8_t *ret,
                                  format_plugin_plugin_error_t *err) {
    (void)err;
    uint64_t start = now_ns();
    buffer_t out = {0};
    reserve(&out, 1);
    for (size_t i = 0; i < entries->len; i++) {
        encode_entry(&out, &entries->ptr[i]);
    }
    free_entries(entries);
    ret->ptr = out.data;
    ret->len = out.len;
    g_last_exec_ns = now_ns() - start;
    return true;
}

uint64_t exports_format_plugin_report_usage(void) {
    return g_last_exec_ns;
}
