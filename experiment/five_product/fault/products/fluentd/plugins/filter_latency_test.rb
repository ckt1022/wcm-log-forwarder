# filter_latency_test.rb — Fluentd Ruby filter（延遲對比實驗）
#
# 邏輯對齊 test/compare/fluentbit/latency/latency_filter.lua，
# 亦即 WCM forwarder 的 plugin pipeline（parse + filter + format）：
#   parse : ts 原字串搬到 timestamp、level 字串/數值映射、msg → message、
#           根層非保留欄位與 att.* 攤平成 tags
#   filter: 只保留 level >= warn（MIN_KEEP_RANK = 2）
#   format: json-flat 欄位集合 id/timestamp/level/message/lang/<tags>
#
# 關鍵約束：timestamp 必須是 gen 原始 ts 字串原封不動，
# HTTP 接收端（common/tools/httpsink）靠這個欄位計算延遲。

require 'fluent/plugin/filter'

module Fluent::Plugin
  class LatencyTestFilter < Filter
    Fluent::Plugin.register_filter('latency_test', self)

    # level 排序，對應 WIT LogLevel enum：debug=0 info=1 warn=2 error=3 crit=4 alert=5 emerg=6
    LEVEL_RANK = {
      'debug' => 0, 'info' => 1, 'warn' => 2, 'error' => 3,
      'crit'  => 4, 'alert' => 5, 'emerg' => 6,
    }.freeze
    LEVEL_NAME = %w[debug info warn error crit alert emerg].freeze
    MIN_KEEP_RANK = 2 # warn（同 filter plugin 的 MIN_KEEP_LEVEL）

    def initialize
      super
      # 對應 forwarder host 端遞增的 entry id（僅為欄位對齊，值本身不影響量測）
      @id_counter = 0
    end

    # 故障注入：msg 含標記時，在處理這一筆時故障
    def inject_fault(msg)
      loop {} if msg.include?('FAULT_LOOP')
      raise 'injected FAULT_PANIC' if msg.include?('FAULT_PANIC')
      [].fetch(1) if msg.include?('FAULT_OOB')
      if msg.include?('FAULT_MEM')
        chunks = []
        loop { chunks << ('x' * 1_048_576) }
      end
    end

    def filter(_tag, _time, record)
      inject_fault(record['msg'].to_s)
      rank = level_rank_of(record['level'])
      return nil if rank < MIN_KEEP_RANK # drop（debug / info）

      @id_counter += 1
      out = {
        'id'        => @id_counter,
        'timestamp' => record['ts'].is_a?(String) ? record['ts'] : '',
        'level'     => LEVEL_NAME[rank],
        'message'   => record['msg'].is_a?(String) ? record['msg'] : '',
        'lang'      => 'Ruby', # 對應 parse plugin 的 lang tag，標識過濾實作
      }

      # 根層非保留欄位（對應 collect_all_tags 的第一段）
      record.each do |k, v|
        next if k == 'ts' || k == 'level' || k == 'msg' || k == 'att'
        out[k] = value_to_string(v)
      end

      # att 物件內欄位攤平（對應 collect_all_tags 的第二段）
      att = record['att']
      att.each { |k, v| out[k] = value_to_string(v) } if att.is_a?(Hash)

      out
    end

    private

    # 對應 parse plugin 的 value_to_string：字串直接取值，其餘轉字串
    def value_to_string(v)
      v.is_a?(String) ? v : v.to_s
    end

    # 對應 parse plugin 的 map_level / level_from_number：
    # 字串不分大小寫映射；數值 0~6 直接取；缺失或未知 → info
    def level_rank_of(level)
      case level
      when String
        LEVEL_RANK[level.downcase] || 1
      when Numeric
        (level >= 0 && level <= 6) ? level.floor : 1
      else
        1
      end
    end
  end
end
