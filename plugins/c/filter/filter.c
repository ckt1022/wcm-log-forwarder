// filter.c — level-based reduction-plugin：保留 level >= warn 的日誌。

#include "reduction_plugin.h"

#include <stdlib.h>
#include <time.h>

static uint64_t g_last_exec_ns = 0;

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

// wit-bindgen（0.58.0 至 0.63.0 皆同）產生的 log_entry_free 不會釋放 tags，
// 只用產生的函式釋放會讓每筆日誌的標籤洩漏，因此先自行釋放 tags。
static void free_entries(reduction_plugin_list_log_entry_t *entries) {
    for (size_t i = 0; i < entries->len; i++) {
        reduction_plugin_list_tuple2_string_string_free(&entries->ptr[i].tags);
    }
    reduction_plugin_list_log_entry_free(entries);
}

// 匯出函式擁有傳入的參數，用完必須釋放；否則 instance 重用時記憶體會一直累積。
bool exports_reduction_plugin_filter(reduction_plugin_list_log_entry_t *entries,
                                     reduction_plugin_list_filter_result_t *ret,
                                     reduction_plugin_plugin_error_t *err) {
    (void)err;
    uint64_t start = now_ns();
    ret->len = entries->len;
    ret->ptr = malloc((entries->len ? entries->len : 1) * sizeof(reduction_plugin_filter_result_t));
    for (size_t i = 0; i < entries->len; i++) {
        ret->ptr[i].id = entries->ptr[i].id;
        ret->ptr[i].keep = entries->ptr[i].level >= LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_WARN;
    }
    free_entries(entries);
    g_last_exec_ns = now_ns() - start;
    return true;
}

uint64_t exports_reduction_plugin_report_usage(void) {
    return g_last_exec_ns;
}
