-- latency_filter.lua — Fluent Bit Lua filter（延遲對比實驗）
--
-- 邏輯對齊 WCM forwarder 的 Rust plugin pipeline（parse + filter + format）：
--   parse : ts 原字串搬到 timestamp、level 字串/數值映射、msg → message、
--           根層非保留欄位與 att.* 攤平成 tags（test-plugins/rust-plugin/parse）
--   filter: 只保留 level >= warn（test-plugins/rust-plugin/filter MIN_KEEP_LEVEL）
--   format: json-flat 欄位集合 id/timestamp/level/message/lang/<tags>
--           （test-plugins/rust-plugin/format 預設 feature）
--
-- 關鍵約束：timestamp 必須是 gen 原始 ts 字串原封不動，
-- HTTP 接收端（common/tools/httpsink）靠這個欄位計算延遲。

-- level 排序，對應 WIT LogLevel enum：debug=0 info=1 warn=2 error=3 crit=4 alert=5 emerg=6
local LEVEL_RANK = {
    debug = 0, info = 1, warn = 2, ["error"] = 3,
    crit = 4, alert = 5, emerg = 6,
}
local LEVEL_NAME = { "debug", "info", "warn", "error", "crit", "alert", "emerg" } -- index = rank+1
local MIN_KEEP_RANK = 2 -- warn（同 filter plugin 的 MIN_KEEP_LEVEL）

-- 對應 parse plugin 的 value_to_string：字串直接取值，其餘轉字串
local function value_to_string(v)
    if type(v) == "string" then
        return v
    end
    return tostring(v)
end

-- 對應 parse plugin 的 map_level / level_from_number：
-- 字串不分大小寫映射；數值 0~6 直接取；缺失或未知 → info
local function level_rank_of(level)
    if type(level) == "string" then
        local r = LEVEL_RANK[string.lower(level)]
        if r ~= nil then return r end
        return 1
    end
    if type(level) == "number" and level >= 0 and level <= 6 then
        return math.floor(level)
    end
    return 1
end

-- 對應 forwarder host 端遞增的 entry id（僅為欄位對齊，值本身不影響量測）
local id_counter = 0

function latency_filter(tag, ts, record)
    local rank = level_rank_of(record["level"])
    if rank < MIN_KEEP_RANK then
        return -1, ts, 0 -- drop（debug / info）
    end

    id_counter = id_counter + 1
    local out = {
        id        = id_counter,
        timestamp = type(record["ts"]) == "string" and record["ts"] or "",
        level     = LEVEL_NAME[rank + 1],
        message   = type(record["msg"]) == "string" and record["msg"] or "",
        lang      = "Lua", -- 對應 parse plugin 的 lang=Rust tag，標識過濾實作
    }

    -- 根層非保留欄位（對應 collect_all_tags 的第一段）
    for k, v in pairs(record) do
        if k ~= "ts" and k ~= "level" and k ~= "msg" and k ~= "att" then
            out[k] = value_to_string(v)
        end
    end

    -- att 物件內欄位攤平（對應 collect_all_tags 的第二段）
    if type(record["att"]) == "table" then
        for k, v in pairs(record["att"]) do
            out[k] = value_to_string(v)
        end
    end

    -- 2 = record 已修改、timestamp（fluent-bit 內部時戳）不變
    return 2, ts, out
end
