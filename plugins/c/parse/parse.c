// parse.c — JSON parser-plugin
//
// 每行一筆 JSON：ts → timestamp、level → level、msg → message，
// 其餘欄位與 att 物件內的欄位攤平成 tags，第一個 tag 固定為 lang=C，targettag 為 "A"。
// 任何一行不是合法 JSON 時，整批回傳 invalid-format，由 host 以二分法找出那一行。

#include "parser_plugin.h"
#include "cJSON.h"

#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <time.h>

static uint64_t g_last_exec_ns = 0;

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static bool is_reserved(const char *key) {
    return strcmp(key, "ts") == 0 || strcmp(key, "level") == 0 || strcmp(key, "msg") == 0 ||
           strcmp(key, "att") == 0;
}

// 字串直接取值，其他型別輸出精簡的 JSON 文字。
static void set_tag_value(parser_plugin_string_t *out, const cJSON *item) {
    if (cJSON_IsString(item)) {
        parser_plugin_string_dup(out, item->valuestring);
        return;
    }
    char *text = cJSON_PrintUnformatted(item);
    parser_plugin_string_dup(out, text ? text : "");
    free(text);
}

static void collect_tags(const cJSON *root, parser_plugin_list_tuple2_string_string_t *tags) {
    const cJSON *att = cJSON_GetObjectItemCaseSensitive(root, "att");
    size_t count = 1;
    const cJSON *item = NULL;
    cJSON_ArrayForEach(item, root) {
        if (!is_reserved(item->string)) {
            count++;
        }
    }
    if (cJSON_IsObject(att)) {
        count += (size_t)cJSON_GetArraySize(att);
    }

    tags->ptr = calloc(count, sizeof(parser_plugin_tuple2_string_string_t));
    tags->len = count;
    parser_plugin_string_dup(&tags->ptr[0].f0, "lang");
    parser_plugin_string_dup(&tags->ptr[0].f1, "C");

    size_t i = 1;
    cJSON_ArrayForEach(item, root) {
        if (is_reserved(item->string)) {
            continue;
        }
        parser_plugin_string_dup(&tags->ptr[i].f0, item->string);
        set_tag_value(&tags->ptr[i].f1, item);
        i++;
    }
    if (cJSON_IsObject(att)) {
        cJSON_ArrayForEach(item, att) {
            parser_plugin_string_dup(&tags->ptr[i].f0, item->string);
            set_tag_value(&tags->ptr[i].f1, item);
            i++;
        }
    }
}

static local_log_process_pipeline_process_log_level_t map_level(const cJSON *level) {
    if (cJSON_IsNumber(level) && level->valueint >= 0 && level->valueint <= 6) {
        return (local_log_process_pipeline_process_log_level_t)level->valueint;
    }
    const char *name = cJSON_IsString(level) ? level->valuestring : NULL;
    if (name == NULL) return LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_INFO;
    if (strcasecmp(name, "debug") == 0) return LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_DEBUG;
    if (strcasecmp(name, "warn") == 0) return LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_WARN;
    if (strcasecmp(name, "error") == 0) return LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_ERROR;
    return LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_INFO;
}

// wit-bindgen 產生的 parsed_entry_free 不會釋放 tags（與 log_entry_free 相同的問題），因此先自行釋放。
static void free_entries(parser_plugin_list_parsed_entry_t *entries) {
    for (size_t i = 0; i < entries->len; i++) {
        parser_plugin_list_tuple2_string_string_free(&entries->ptr[i].tags);
    }
    parser_plugin_list_parsed_entry_free(entries);
}

static bool parse_lines(parser_plugin_list_string_t *raw, parser_plugin_list_parsed_entry_t *ret,
                        parser_plugin_parse_error_t *err) {
    ret->len = raw->len;
    ret->ptr = calloc(raw->len ? raw->len : 1, sizeof(parser_plugin_parsed_entry_t));

    for (size_t i = 0; i < raw->len; i++) {
        cJSON *root = cJSON_ParseWithLength((const char *)raw->ptr[i].ptr, raw->ptr[i].len);
        if (root == NULL) {
            free_entries(ret);
            err->tag = LOCAL_LOG_PROCESS_PIPELINE_PROCESS_PARSE_ERROR_INVALID_FORMAT;
            parser_plugin_string_dup(&err->val.invalid_format, "JSON parse error");
            return false;
        }
        parser_plugin_parsed_entry_t *entry = &ret->ptr[i];
        const cJSON *ts = cJSON_GetObjectItemCaseSensitive(root, "ts");
        const cJSON *msg = cJSON_GetObjectItemCaseSensitive(root, "msg");
        parser_plugin_string_dup(&entry->timestamp, cJSON_IsString(ts) ? ts->valuestring : "");
        entry->level = map_level(cJSON_GetObjectItemCaseSensitive(root, "level"));
        parser_plugin_string_dup(&entry->message, cJSON_IsString(msg) ? msg->valuestring : "");
        collect_tags(root, &entry->tags);
        parser_plugin_string_dup(&entry->targettag, "A");
        cJSON_Delete(root);
    }
    return true;
}

// 匯出函式擁有傳入的參數，用完必須釋放；否則 instance 重用時記憶體會一直累積。
bool exports_parser_plugin_parse(parser_plugin_list_string_t *raw_data, parser_plugin_list_parsed_entry_t *ret,
                                 parser_plugin_parse_error_t *err) {
    uint64_t start = now_ns();
    bool ok = parse_lines(raw_data, ret, err);
    parser_plugin_list_string_free(raw_data);
    g_last_exec_ns = now_ns() - start;
    return ok;
}

uint64_t exports_parser_plugin_report_usage(void) {
    return g_last_exec_ns;
}
