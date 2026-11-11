// transport.c — wasi:http transport-plugin
//
// 以 wasi:http/outgoing-handler 送出 HTTP POST。host 負責 TCP 連線、TLS 與 HTTP framing，
// 也只放行 forwarder.yaml `endpoint:` 中的位址；插件只描述請求並寫入 body。
// 重試由 host 負責（init 收到的 retry 為 none 時只送一次）。

#include "transport_plugin.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// ───────────────────────────────────────────────────────────
// 動態位元組緩衝區
// ───────────────────────────────────────────────────────────

typedef struct {
    uint8_t *data;
    size_t len;
    size_t cap;
} byte_buf_t;

static void bb_init(byte_buf_t *b) {
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
}

static void bb_reserve(byte_buf_t *b, size_t extra) {
    if (b->len + extra <= b->cap) return;
    size_t ncap = b->cap ? b->cap * 2 : 256;
    while (ncap < b->len + extra) ncap *= 2;
    b->data = (uint8_t*)realloc(b->data, ncap);
    b->cap = ncap;
}

static void bb_append(byte_buf_t *b, const uint8_t *p, size_t l) {
    if (l == 0) return;
    bb_reserve(b, l);
    memcpy(b->data + b->len, p, l);
    b->len += l;
}

static void bb_free(byte_buf_t *b) {
    free(b->data);
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
}

typedef struct {
    byte_buf_t *items;
    size_t count;
    size_t cap;
} batch_list_t;

static void bl_push(batch_list_t *bl, byte_buf_t bb) {
    if (bl->count == bl->cap) {
        bl->cap = bl->cap ? bl->cap * 2 : 4;
        bl->items = (byte_buf_t*)realloc(bl->items, bl->cap * sizeof(byte_buf_t));
    }
    bl->items[bl->count++] = bb;
}

// ───────────────────────────────────────────────────────────
// 插件狀態
// ───────────────────────────────────────────────────────────

typedef struct {
    uint8_t tag;
    char *bearer_token;
    char *basic_username;
    char *basic_password;
    char *apikey_header;
    char *apikey_key;
} auth_state_t;

typedef struct {
    char **names;
    char **values;
    size_t count;
} header_list_t;

typedef struct {
    bool initialized;

    char *scheme;   // "http" or "https"
    char *host;
    uint16_t port;
    char *path;
    char *authority; // "host" or "host:port"

    auth_state_t auth;

    bool has_retry;
    uint32_t max_retries;
    uint32_t initial_backoff_ms;
    uint32_t max_backoff_ms;

    uint32_t request_timeout_ms;
    uint32_t max_batch_bytes;

    header_list_t extra_headers;

    uint64_t bytes_sent;
} state_t;

static state_t g_state = {0};

static void free_state(state_t *s) {
    free(s->scheme);
    free(s->host);
    free(s->path);
    free(s->authority);
    free(s->auth.bearer_token);
    free(s->auth.basic_username);
    free(s->auth.basic_password);
    free(s->auth.apikey_header);
    free(s->auth.apikey_key);
    for (size_t i = 0; i < s->extra_headers.count; i++) {
        free(s->extra_headers.names[i]);
        free(s->extra_headers.values[i]);
    }
    free(s->extra_headers.names);
    free(s->extra_headers.values);
    memset(s, 0, sizeof(*s));
}

// ───────────────────────────────────────────────────────────
// 小工具：字串 / 錯誤 / base64 / clock
// ───────────────────────────────────────────────────────────

static char* dup_wstr(transport_plugin_string_t s) {
    char *r = (char*)malloc(s.len + 1);
    memcpy(r, s.ptr, s.len);
    r[s.len] = '\0';
    return r;
}

static char* dup_cstr(const char *s) {
    size_t l = strlen(s);
    char *r = (char*)malloc(l + 1);
    memcpy(r, s, l + 1);
    return r;
}

static void set_err_config(transport_plugin_plugin_error_t *err, const char *msg) {
    err->tag = LOCAL_LOG_PROCESS_PIPELINE_PROCESS_PLUGIN_ERROR_CONFIG_ERROR;
    transport_plugin_string_dup(&err->val.config_error, msg);
}

static void set_err_processing(transport_plugin_plugin_error_t *err, const char *msg) {
    err->tag = LOCAL_LOG_PROCESS_PIPELINE_PROCESS_PLUGIN_ERROR_PROCESSING_FAILED;
    transport_plugin_string_dup(&err->val.processing_failed, msg);
}

// 把 a、十進位數字 n、b 串成一個新字串。
static char* concat_num(const char *a, uint32_t n, const char *b) {
    char digits[11];
    size_t d = sizeof(digits);
    digits[--d] = '\0';
    do {
        digits[--d] = (char)('0' + n % 10);
        n /= 10;
    } while (n > 0);
    size_t la = strlen(a), ln = strlen(digits + d), lb = strlen(b);
    char *r = (char*)malloc(la + ln + lb + 1);
    memcpy(r, a, la);
    memcpy(r + la, digits + d, ln);
    memcpy(r + la + ln, b, lb + 1);
    return r;
}

static char* concat(const char *a, const char *b) {
    size_t la = strlen(a), lb = strlen(b);
    char *r = (char*)malloc(la + lb + 1);
    memcpy(r, a, la);
    memcpy(r + la, b, lb + 1);
    return r;
}

static void set_err_processing_num(transport_plugin_plugin_error_t *err, const char *a, uint32_t n, const char *b) {
    char *msg = concat_num(a, n, b);
    set_err_processing(err, msg);
    free(msg);
}

static void sleep_ms(uint64_t ms) {
    wasi_clocks_monotonic_clock_own_pollable_t p =
        wasi_clocks_monotonic_clock_subscribe_duration(ms * 1000000ULL);
    wasi_io_poll_borrow_pollable_t pb = wasi_io_poll_borrow_pollable(p);
    wasi_io_poll_method_pollable_block(pb);
    wasi_io_poll_pollable_drop_own(p);
}

static char* base64_encode(const uint8_t *input, size_t len) {
    static const char *TABLE = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    size_t out_len = ((len + 2) / 3) * 4;
    char *out = (char*)malloc(out_len + 1);
    size_t oi = 0;
    for (size_t i = 0; i < len; i += 3) {
        uint8_t b0 = input[i];
        uint8_t b1 = (i + 1 < len) ? input[i + 1] : 0;
        uint8_t b2 = (i + 2 < len) ? input[i + 2] : 0;
        out[oi++] = TABLE[b0 >> 2];
        out[oi++] = TABLE[((b0 & 0x3) << 4) | (b1 >> 4)];
        out[oi++] = (i + 1 < len) ? TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] : '=';
        out[oi++] = (i + 2 < len) ? TABLE[b2 & 0x3f] : '=';
    }
    out[oi] = '\0';
    return out;
}

// ───────────────────────────────────────────────────────────
// endpoint 解析：http://host[:port][/path] 或 https://...
// ───────────────────────────────────────────────────────────

static bool parse_endpoint(const char *endpoint,
                            char **out_scheme, char **out_host,
                            uint16_t *out_port, char **out_path,
                            char **out_authority,
                            transport_plugin_plugin_error_t *err) {
    const char *rest;
    const char *scheme_str;
    uint16_t default_port;

    if (strncmp(endpoint, "http://", 7) == 0) {
        rest = endpoint + 7;
        scheme_str = "http";
        default_port = 80;
    } else if (strncmp(endpoint, "https://", 8) == 0) {
        rest = endpoint + 8;
        scheme_str = "https";
        default_port = 443;
    } else {
        set_err_config(err, "unsupported scheme (only http:// and https:// are supported)");
        return false;
    }

    const char *slash = strchr(rest, '/');
    size_t authority_len = slash ? (size_t)(slash - rest) : strlen(rest);
    const char *path_src = slash ? slash : "/";

    if (authority_len == 0) {
        set_err_config(err, "endpoint is missing a host");
        return false;
    }

    char authority_raw[256];
    if (authority_len >= sizeof(authority_raw)) authority_len = sizeof(authority_raw) - 1;
    memcpy(authority_raw, rest, authority_len);
    authority_raw[authority_len] = '\0';

    char *colon = strrchr(authority_raw, ':');
    uint16_t port = default_port;
    char host[256];
    if (colon) {
        size_t hlen = (size_t)(colon - authority_raw);
        if (hlen >= sizeof(host)) hlen = sizeof(host) - 1;
        memcpy(host, authority_raw, hlen);
        host[hlen] = '\0';
        int p = atoi(colon + 1);
        if (p > 0 && p <= 65535) port = (uint16_t)p;
    } else {
        strncpy(host, authority_raw, sizeof(host) - 1);
        host[sizeof(host) - 1] = '\0';
    }

    if (host[0] == '\0') {
        set_err_config(err, "endpoint host must not be empty");
        return false;
    }

    // authority for the Host/authority field: omit default port
    bool is_default = (strcmp(scheme_str, "http") == 0 && port == 80)
                   || (strcmp(scheme_str, "https") == 0 && port == 443);
    char *host_colon = concat(host, ":");
    char *authority = is_default ? dup_cstr(host) : concat_num(host_colon, port, "");
    free(host_colon);

    *out_scheme    = dup_cstr(scheme_str);
    *out_host      = dup_cstr(host);
    *out_port      = port;
    *out_path      = dup_cstr(path_src);
    *out_authority = authority;
    return true;
}

// ───────────────────────────────────────────────────────────
// 送出單一 HTTP 請求（wasi:http outgoing-handler）
// ───────────────────────────────────────────────────────────

static bool send_http_request(const uint8_t *body, size_t body_len,
                               transport_plugin_plugin_error_t *err) {
    wasi_http_types_header_error_t hdrerr;

    // ── 1. 建立請求 headers ──────────────────────────────────────────────────
    wasi_http_types_own_fields_t hdrs_own = wasi_http_types_constructor_fields();
    wasi_http_types_borrow_fields_t hdrs_b = wasi_http_types_borrow_fields(hdrs_own);

    // Content-Type
    {
        wasi_http_types_field_key_t k;
        transport_plugin_string_set(&k, "content-type");
        wasi_http_types_field_value_t v = {
            .ptr = (uint8_t*)"application/octet-stream", .len = 24
        };
        wasi_http_types_method_fields_append(hdrs_b, &k, &v, &hdrerr);
    }

    // Auth
    switch (g_state.auth.tag) {
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_BEARER_TOKEN: {
            char *v_str = concat("Bearer ", g_state.auth.bearer_token);
            wasi_http_types_field_key_t k;
            transport_plugin_string_set(&k, "authorization");
            wasi_http_types_field_value_t v = {
                .ptr = (uint8_t*)v_str, .len = strlen(v_str)
            };
            wasi_http_types_method_fields_append(hdrs_b, &k, &v, &hdrerr);
            free(v_str);
            break;
        }
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_BASIC_AUTH: {
            char *user_colon = concat(g_state.auth.basic_username, ":");
            char *cred = concat(user_colon, g_state.auth.basic_password);
            char *enc = base64_encode((uint8_t*)cred, strlen(cred));
            char *v_str = concat("Basic ", enc);
            free(user_colon);
            free(cred);
            free(enc);
            wasi_http_types_field_key_t k;
            transport_plugin_string_set(&k, "authorization");
            wasi_http_types_field_value_t v = {
                .ptr = (uint8_t*)v_str, .len = strlen(v_str)
            };
            wasi_http_types_method_fields_append(hdrs_b, &k, &v, &hdrerr);
            free(v_str);
            break;
        }
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_API_KEY: {
            wasi_http_types_field_key_t k;
            transport_plugin_string_set(&k, g_state.auth.apikey_header);
            const char *val_str = g_state.auth.apikey_key;
            wasi_http_types_field_value_t v = {
                .ptr = (uint8_t*)val_str, .len = strlen(val_str)
            };
            wasi_http_types_method_fields_append(hdrs_b, &k, &v, &hdrerr);
            break;
        }
        default:
            break;
    }

    // Extra headers
    for (size_t i = 0; i < g_state.extra_headers.count; i++) {
        wasi_http_types_field_key_t k;
        transport_plugin_string_set(&k, g_state.extra_headers.names[i]);
        const char *val_str = g_state.extra_headers.values[i];
        wasi_http_types_field_value_t v = {
            .ptr = (uint8_t*)val_str, .len = strlen(val_str)
        };
        wasi_http_types_method_fields_append(hdrs_b, &k, &v, &hdrerr);
    }

    // ── 2. 建立 OutgoingRequest（消耗 hdrs_own）──────────────────────────────
    wasi_http_types_own_outgoing_request_t req_own =
        wasi_http_types_constructor_outgoing_request(hdrs_own);
    wasi_http_types_borrow_outgoing_request_t req_b =
        wasi_http_types_borrow_outgoing_request(req_own);

    // Method = POST
    wasi_http_types_method_t method = { .tag = WASI_HTTP_TYPES_METHOD_POST };
    wasi_http_types_method_outgoing_request_set_method(req_b, &method);

    // Scheme
    wasi_http_types_scheme_t scheme;
    if (strcmp(g_state.scheme, "https") == 0) {
        scheme.tag = WASI_HTTP_TYPES_SCHEME_HTTPS;
    } else {
        scheme.tag = WASI_HTTP_TYPES_SCHEME_HTTP;
    }
    wasi_http_types_method_outgoing_request_set_scheme(req_b, &scheme);

    // Authority (Host)
    transport_plugin_string_t authority_str;
    transport_plugin_string_set(&authority_str, g_state.authority);
    wasi_http_types_method_outgoing_request_set_authority(req_b, &authority_str);

    // Path
    transport_plugin_string_t path_str;
    transport_plugin_string_set(&path_str, g_state.path);
    wasi_http_types_method_outgoing_request_set_path_with_query(req_b, &path_str);

    // ── 3. 取得 body 的 output stream ────────────────────────────────────────
    wasi_http_types_own_outgoing_body_t body_own;
    if (!wasi_http_types_method_outgoing_request_body(req_b, &body_own)) {
        wasi_http_types_outgoing_request_drop_own(req_own);
        set_err_processing(err, "failed to get outgoing body handle");
        return false;
    }
    wasi_http_types_borrow_outgoing_body_t body_b =
        wasi_http_types_borrow_outgoing_body(body_own);

    wasi_http_types_own_output_stream_t stream_own;
    if (!wasi_http_types_method_outgoing_body_write(body_b, &stream_own)) {
        wasi_http_types_outgoing_body_drop_own(body_own);
        wasi_http_types_outgoing_request_drop_own(req_own);
        set_err_processing(err, "failed to get outgoing body stream");
        return false;
    }
    wasi_io_streams_borrow_output_stream_t stream_b =
        wasi_io_streams_borrow_output_stream(stream_own);

    // ── 4. 先送出請求（消耗 req_own）────────────────────────────────────────
    // 與 transport_http 不同：先 handle() 再串流寫 body。
    // handle() 之前寫入的 body 只能暫存在 host 的 4096B 緩衝，超過即 deadlock；
    // 先 handle() 讓 host 開始消耗 stream，body 大小就不再受限。
    wasi_http_outgoing_handler_own_future_incoming_response_t future_own;
    wasi_http_outgoing_handler_error_code_t handle_err;
    if (!wasi_http_outgoing_handler_handle(req_own, NULL, &future_own, &handle_err)) {
        // req_own 被消耗（即使失敗）
        wasi_io_streams_output_stream_drop_own(stream_own);
        wasi_http_types_outgoing_body_drop_own(body_own);
        set_err_processing(err, "outgoing-handler.handle() failed");
        return false;
    }

    // ── 5. 分批寫入 body（blocking_write_and_flush 每次最多 4096 B）──────────
    const size_t CHUNK = 4096;
    size_t offset = 0;
    bool write_ok = true;
    while (offset < body_len) {
        size_t end = offset + CHUNK;
        if (end > body_len) end = body_len;
        transport_plugin_list_u8_t chunk = {
            .ptr = (uint8_t*)body + offset, .len = end - offset
        };
        wasi_io_streams_stream_error_t serr;
        if (!wasi_io_streams_method_output_stream_blocking_write_and_flush(
                stream_b, &chunk, &serr)) {
            set_err_processing_num(err, "body write failed (stream error tag ", serr.tag, ")");
            write_ok = false;
            break;
        }
        offset = end;
    }

    // 必須在 finish 前 drop stream
    wasi_io_streams_output_stream_drop_own(stream_own);

    if (!write_ok) {
        wasi_http_types_outgoing_body_drop_own(body_own);
        wasi_http_types_future_incoming_response_drop_own(future_own);
        return false;
    }

    // ── 6. Finish body（消耗 body_own，無 trailers）──────────────────────────
    wasi_http_types_error_code_t finish_err;
    if (!wasi_http_types_static_outgoing_body_finish(body_own, NULL, &finish_err)) {
        // body_own 被消耗（即使失敗）
        wasi_http_types_future_incoming_response_drop_own(future_own);
        set_err_processing(err, "failed to finish outgoing body");
        return false;
    }

    // ── 7. 等待回應（poll）──────────────────────────────────────────────────
    wasi_http_types_borrow_future_incoming_response_t future_b =
        wasi_http_types_borrow_future_incoming_response(future_own);

    wasi_http_types_own_pollable_t poll_own =
        wasi_http_types_method_future_incoming_response_subscribe(future_b);
    wasi_io_poll_borrow_pollable_t poll_b = wasi_io_poll_borrow_pollable(poll_own);
    wasi_io_poll_method_pollable_block(poll_b);
    wasi_io_poll_pollable_drop_own(poll_own);

    // ── 8. 取得回應並檢查 status code ───────────────────────────────────────
    wasi_http_types_result_result_own_incoming_response_error_code_void_t result;
    bool got = wasi_http_types_method_future_incoming_response_get(future_b, &result);
    wasi_http_types_future_incoming_response_drop_own(future_own);

    if (!got) {
        set_err_processing(err, "future not ready after polling");
        return false;
    }
    // result 的外層 is_err = true 代表 option::none（理論上 poll 後不應發生）
    if (result.is_err) {
        set_err_processing(err, "HTTP future returned no value after poll");
        return false;
    }
    // 內層 is_err = true 代表 wasi:http 層級的錯誤（連線失敗等）
    if (result.val.ok.is_err) {
        set_err_processing_num(err, "HTTP request failed (error-code tag ", result.val.ok.val.err.tag, ")");
        return false;
    }

    wasi_http_types_own_incoming_response_t resp_own = result.val.ok.val.ok;
    wasi_http_types_borrow_incoming_response_t resp_b =
        wasi_http_types_borrow_incoming_response(resp_own);
    uint16_t status = wasi_http_types_method_incoming_response_status(resp_b);
    wasi_http_types_incoming_response_drop_own(resp_own);

    if (status < 200 || status >= 300) {
        // host 依 "HTTP <status>" 判斷是否值得重試
        set_err_processing_num(err, "server returned HTTP ", status, "");
        return false;
    }

    return true;
}

// ───────────────────────────────────────────────────────────
// 帶重試的送出
// ───────────────────────────────────────────────────────────

static bool send_with_retry(const uint8_t *body, size_t len,
                             transport_plugin_plugin_error_t *err) {
    uint32_t max_retries    = g_state.has_retry ? g_state.max_retries : 0;
    uint64_t backoff_ms     = g_state.has_retry ? g_state.initial_backoff_ms : 100;
    uint64_t max_backoff_ms = g_state.has_retry ? g_state.max_backoff_ms     : 5000;
    uint32_t attempt = 0;

    for (;;) {
        transport_plugin_plugin_error_t local_err;
        if (send_http_request(body, len, &local_err)) return true;
        if (attempt < max_retries) {
            attempt++;
            transport_plugin_plugin_error_free(&local_err);
            sleep_ms(backoff_ms);
            backoff_ms *= 2;
            if (backoff_ms > max_backoff_ms) backoff_ms = max_backoff_ms;
            continue;
        }
        *err = local_err;
        return false;
    }
}

// ───────────────────────────────────────────────────────────
// batch 切割（依 max-batch-bytes，0 = 不限制）
// ───────────────────────────────────────────────────────────

static void build_batches(transport_plugin_list_list_u8_t *output_data,
                          uint32_t max_batch_bytes, batch_list_t *out) {
    out->items = NULL;
    out->count = 0;
    out->cap   = 0;

    if (max_batch_bytes == 0) {
        byte_buf_t buf;
        bb_init(&buf);
        for (size_t i = 0; i < output_data->len; i++) {
            transport_plugin_list_u8_t *entry = &output_data->ptr[i];
            bb_append(&buf, entry->ptr, entry->len);
            if (entry->len == 0 || entry->ptr[entry->len - 1] != '\n') {
                bb_append(&buf, (const uint8_t*)"\n", 1);
            }
        }
        bl_push(out, buf);
        return;
    }

    size_t limit = max_batch_bytes;
    byte_buf_t cur;
    bb_init(&cur);
    for (size_t i = 0; i < output_data->len; i++) {
        transport_plugin_list_u8_t *entry = &output_data->ptr[i];
        size_t entry_size = entry->len + 1;
        if (cur.len > 0 && cur.len + entry_size > limit) {
            bl_push(out, cur);
            bb_init(&cur);
        }
        bb_append(&cur, entry->ptr, entry->len);
        if (entry->len == 0 || entry->ptr[entry->len - 1] != '\n') {
            bb_append(&cur, (const uint8_t*)"\n", 1);
        }
    }
    if (cur.len > 0) {
        bl_push(out, cur);
    } else {
        bb_free(&cur);
    }
}

// ───────────────────────────────────────────────────────────
// Exports
// ───────────────────────────────────────────────────────────

static bool init_state(transport_plugin_transport_config_t *config,
                       transport_plugin_plugin_error_t *err) {
    free_state(&g_state);

    if (config->endpoint.len == 0) {
        set_err_config(err, "endpoint must not be empty");
        return false;
    }

    char *endpoint = dup_wstr(config->endpoint);
    char *scheme = NULL, *host = NULL, *path = NULL, *authority = NULL;
    uint16_t port;
    bool ok = parse_endpoint(endpoint, &scheme, &host, &port, &path, &authority, err);
    free(endpoint);
    if (!ok) return false;

    g_state.scheme    = scheme;
    g_state.host      = host;
    g_state.port      = port;
    g_state.path      = path;
    g_state.authority = authority;

    g_state.auth.tag = config->auth.tag;
    switch (config->auth.tag) {
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_BEARER_TOKEN:
            g_state.auth.bearer_token = dup_wstr(config->auth.val.bearer_token);
            break;
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_BASIC_AUTH:
            g_state.auth.basic_username = dup_wstr(config->auth.val.basic_auth.username);
            g_state.auth.basic_password = dup_wstr(config->auth.val.basic_auth.password);
            break;
        case LOCAL_LOG_PROCESS_TRANSPORT_TYPES_AUTH_METHOD_API_KEY:
            g_state.auth.apikey_header = dup_wstr(config->auth.val.api_key.header_name);
            g_state.auth.apikey_key    = dup_wstr(config->auth.val.api_key.key);
            break;
        default:
            break;
    }

    g_state.has_retry = config->retry.is_some;
    if (g_state.has_retry) {
        g_state.max_retries        = config->retry.val.max_retries;
        g_state.initial_backoff_ms = config->retry.val.initial_backoff_ms;
        g_state.max_backoff_ms     = config->retry.val.max_backoff_ms;
    }

    g_state.request_timeout_ms = config->request_timeout_ms;
    g_state.max_batch_bytes    = config->max_batch_bytes;

    size_t n = config->extra_headers.len;
    if (n > 0) {
        g_state.extra_headers.names  = (char**)malloc(n * sizeof(char*));
        g_state.extra_headers.values = (char**)malloc(n * sizeof(char*));
        for (size_t i = 0; i < n; i++) {
            g_state.extra_headers.names[i]  = dup_wstr(config->extra_headers.ptr[i].f0);
            g_state.extra_headers.values[i] = dup_wstr(config->extra_headers.ptr[i].f1);
        }
        g_state.extra_headers.count = n;
    }

    g_state.bytes_sent   = 0;
    g_state.initialized  = true;
    return true;
}

static bool send_all(transport_plugin_list_list_u8_t *output_data,
                     transport_plugin_plugin_error_t *err) {
    if (!g_state.initialized) {
        set_err_config(err, "init() not called before send()");
        return false;
    }
    if (output_data->len == 0) return true;

    batch_list_t batches;
    build_batches(output_data, g_state.max_batch_bytes, &batches);

    uint64_t total_sent = 0;
    bool all_ok = true;
    for (size_t i = 0; i < batches.count; i++) {
        byte_buf_t *b = &batches.items[i];
        if (!send_with_retry(b->data, b->len, err)) {
            all_ok = false;
            break;
        }
        total_sent += b->len;
    }

    for (size_t i = 0; i < batches.count; i++) bb_free(&batches.items[i]);
    free(batches.items);

    if (!all_ok) return false;

    g_state.bytes_sent += total_sent;
    return true;
}

// wit-bindgen（0.58.0 至 0.63.0 皆同）產生的 list_list_u8_free 只釋放外層陣列，
// 不會釋放每一行的資料，因此逐行自行釋放。
static void free_lines(transport_plugin_list_list_u8_t *lines) {
    for (size_t i = 0; i < lines->len; i++) {
        transport_plugin_list_u8_free(&lines->ptr[i]);
    }
    transport_plugin_list_list_u8_free(lines);
}

// 匯出函式擁有傳入的參數，用完必須釋放；否則 instance 重用時記憶體會一直累積。
bool exports_transport_plugin_init(transport_plugin_transport_config_t *config,
                                    transport_plugin_plugin_error_t *err) {
    bool ok = init_state(config, err);
    transport_plugin_transport_config_free(config);
    return ok;
}

bool exports_transport_plugin_transport(transport_plugin_list_list_u8_t *output_data,
                                        transport_plugin_plugin_error_t *err) {
    bool ok = send_all(output_data, err);
    free_lines(output_data);
    return ok;
}

uint64_t exports_transport_plugin_report_usage(void) {
    return g_state.bytes_sent;
}
