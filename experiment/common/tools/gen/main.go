// Log generator for wcm-base-log-forwarder testing and benchmarking.
//
// Usage:
//
//	go run main.go [flags] | ../target/debug/wcm-base-log-forwarder
//
// Modes:
//
//	json-simple    - 固定格式 JSON，少量 att 欄位（基本效能測試）
//	json-complex   - JSON 含多個 att 欄位與較長訊息（壓力測試）
//	json-mixed     - json-simple + json-complex 混合（接近真實情況）
//	json-fixed5    - 5 條固定內容循環（約 160 B）
//	json-fixed256  - 每行固定 256 B，維持與 json-fixed5 相同的 5 個 top-level 欄位
//	json-fixed512  - 每行固定 512 B，維持與 json-fixed5 相同的 5 個 top-level 欄位
//	json-fields256 - 每行固定 256 B，8–9 個 top-level JSON 欄位
//	json-fields512 - 每行固定 512 B，以約 1 KiB 多欄位模式一半的欄位數擴充
//	json-fields128 - 每行固定 128 B，4 個 top-level JSON 欄位（ts/level/msg/att）
//	json-fields768 - 每行固定 768 B，27 個 top-level JSON 欄位
//	json-fixed1k   - 與 json-fixed5 相同內容分布，每行固定 1024 B
//	json-fields1k  - 每行固定 1024 B，以多個獨立 JSON 欄位擴充（無 payload）
//	json-fields1536 - 每行固定 1536 B，54 個 top-level JSON 欄位
//	json-fixed2k   - 與 json-fixed5 相同內容分布，每行固定 2048 B
//	json-fields2k  - 每行固定 2048 B，72 個 top-level JSON 欄位
//	invalid        - 混入無法解析的行（測試 parser 錯誤處理；valid line 為 json-simple）
//	logfmt-simple  - 扁平 key=value 格式，基本效能測試
//	logfmt-complex - logfmt 含較多欄位與較長訊息
//	logfmt-mixed   - logfmt simple + complex 混合
//	syslog-simple  - RFC5424 風格 syslog，基本欄位
//	syslog-complex - RFC5424 風格 syslog，附帶延伸 key=value 欄位
//	syslog-mixed   - syslog simple + complex 混合
//
// Traffic shapes (-traffic):
//
//	flat   - 固定速率（預設）
//	wave   - 正弦波形，高峰低谷交替，長期平均等於 -rate（適合可控實驗）
//	bursty - 正弦波底層＋隨機突發尖峰，模擬真實流量（適合壓力測試）
package main

import (
	"bufio"
	"flag"
	"fmt"
	"math"
	"math/rand"
	"net"
	"os"
	"strconv"
	"strings"
	"time"
)

var (
	rate        = flag.Int("rate", 5000, "target lines per second")
	duration    = flag.Int("duration", 30, "run duration in seconds, 0=forever")
	mode        = flag.String("mode", "json-simple", "log mode: json-simple|json-complex|json-mixed|json-fixed5|json-fixed256|json-fixed512|json-fields128|json-fields256|json-fields512|json-fields768|json-fixed1k|json-fields1k|json-fields1536|json-fixed2k|json-fields2k|invalid|logfmt-simple|logfmt-complex|logfmt-mixed|syslog-simple|syslog-complex|syslog-mixed")
	invalidRate = flag.Float64("invalid-rate", 0.05, "fraction of invalid/malformed lines when mode=invalid (0.0-1.0)")
	// [pattern_gaps G5] 任何 mode 都可混入 1024 B 的 malformed 行（預設 0 = 關閉，行為與舊版完全相同；不使用 rand，不改變亂數序列）
	malformedRate = flag.Float64("malformed-rate", 0, "fraction (0.0-1.0) of lines replaced by deterministic 1024-byte malformed lines in ANY mode (seq hash, same pattern as correctness generator); 0 = off (default)")
	bufferSize    = flag.Int("buffer", 1<<20, "stdout buffer size in bytes (default 1MB)")
	flushEvery    = flag.Int("flush-ms", 100, "flush stdout interval in milliseconds")
	seed          = flag.Int64("seed", 0, "random seed, 0=use current time")

	// Traffic shape flags
	trafficShape = flag.String("traffic", "flat", "traffic shape: flat|wave|bursty")
	waveAmp      = flag.Float64("wave-amp", 0.6, "sine wave amplitude 0.0-0.9 (wave/bursty); rate varies between rate*(1-amp) and rate*(1+amp)")
	wavePeriod   = flag.Float64("wave-period", 60.0, "sine wave period in seconds")
	spikeMult    = flag.Float64("spike-mult", 3.0, "spike peak multiplier applied on top of wave (bursty)")
	spikeFreqPM  = flag.Float64("spike-freq", 2.0, "average spikes per minute (bursty)")
	spikeDurSec  = flag.Float64("spike-dur", 5.0, "spike duration in seconds (bursty)")

	// Output flags
	logFile = flag.String("log-file", "gen.log", "write generator diagnostics to this file; use '-' for stderr")

	// TCP output flags
	outputDest = flag.String("output", "stdout", "output destination: stdout | tcp")
	tcpAddr    = flag.String("tcp-addr", "127.0.0.1:5140", "forwarder TCP address when output=tcp")
	sendUnit   = flag.String("send-unit", "line", "send granularity when output=tcp: line (每產生一條立即送出) | batch (累積 N 行再送)")
	batchLines = flag.Int("batch-lines", 100, "lines per batch when send-unit=batch")

	// High-rate / measurement-bias flags
	tsMode = flag.String("ts-mode", "actual", "ts 欄位的時間戳來源: actual=實際送出時間（舊行為，預設）| sched=排程時間（高負載延遲量測用，避免 coordinated omission：DUT backpressure 卡住 gen 時，排隊時間會被算進延遲而不是被晚蓋的時間戳藏掉）")
	conns  = flag.Int("conns", 1, "output=tcp 時的平行 TCP 連線數（round-robin 分配行）")
)

// -----------------------------------------------------------------
// Log field pools
// -----------------------------------------------------------------

var levels = []string{"debug", "info", "info", "info", "warn", "error"}

var services = []string{"api-gateway", "auth-service", "payment-svc", "order-svc", "notify-svc"}

var simpleMessages = []string{
	"Connection accepted from 192.168.1.100",
	"Request completed successfully",
	"Cache hit for key user_session",
	"Database query executed in 3ms",
	"Health check passed",
}

var complexMessages = []string{
	"Upstream timeout after 30s waiting for response from backend pool eu-west-2",
	"Token validation failed: signature mismatch, expected HS256 but received RS256 algorithm",
	"Retry attempt 3/5 for POST /api/v2/orders after connection reset by peer",
	"Circuit breaker OPEN for service payment-svc: 15 failures in last 60s window",
	"Slow query detected: SELECT * FROM events WHERE user_id=? AND created_at>? took 1240ms",
}

var httpCodes = []string{"200", "201", "400", "401", "403", "404", "429", "500", "502", "503"}
var regions = []string{"us-east-1", "eu-west-1", "ap-southeast-1"}
var traceIDs = []string{"4bf92f3577b34da6", "00f067aa0ba902b7", "e457b5a2e4d86bd1", "1234567890abcdef"}

// -----------------------------------------------------------------
// Line builders - JSON
// -----------------------------------------------------------------

// simpleJSONLineLen is the fixed byte length for every json-simple line.
// Computed from max natural content (149 B) + padding field overhead (11 B) + margin (12 B).
// All lines are padded to exactly this length via a trailing "_p" field.
const simpleJSONLineLen = 172

// simpleJSONPad is a pre-allocated space string used for padding; length must be >= 50.
const simpleJSONPad = "                                                                " // 64 spaces

func buildSimpleJSONLine(dst []byte, ts, level, svc, code string) []byte {
	dst = dst[:0]
	dst = append(dst, `{"ts":"`...)
	dst = append(dst, ts...)
	dst = append(dst, `","level":"`...)
	dst = append(dst, level...)
	dst = append(dst, `","msg":"`...)
	dst = append(dst, simpleMessages[rand.Intn(len(simpleMessages))]...)
	dst = append(dst, `","att":{"service":"`...)
	dst = append(dst, svc...)
	dst = append(dst, `","code":"`...)
	dst = append(dst, code...)
	dst = append(dst, `"},"_p":"`...)
	padLen := simpleJSONLineLen - len(dst) - 2 // -2 for closing `"}`
	if padLen < 0 {
		padLen = 0
	}
	dst = append(dst, simpleJSONPad[:padLen]...)
	dst = append(dst, `"}`...)
	return dst
}

func buildComplexJSONLine(dst []byte, ts, level, svc, code, region, traceID string, latencyMs int) []byte {
	dst = dst[:0]
	dst = append(dst, `{"ts":"`...)
	dst = append(dst, ts...)
	dst = append(dst, `","level":"`...)
	dst = append(dst, level...)
	dst = append(dst, `","msg":"`...)
	dst = append(dst, complexMessages[rand.Intn(len(complexMessages))]...)
	dst = append(dst, `","att":{"service":"`...)
	dst = append(dst, svc...)
	dst = append(dst, `","code":"`...)
	dst = append(dst, code...)
	dst = append(dst, `","region":"`...)
	dst = append(dst, region...)
	dst = append(dst, `","trace_id":"`...)
	dst = append(dst, traceID...)
	dst = fmt.Appendf(dst, `","latency_ms":"%d","host":"worker-%02d","env":"production"}}`, latencyMs, rand.Intn(32))
	return dst
}

// -----------------------------------------------------------------
// json-fixed5: 5 條固定內容循環（ts 每次刷新，_slot 標識種類）
// -----------------------------------------------------------------

// fixed5Tpl 每個元素對應一條固定 log 的靜態部分（不含 ts）。
// 格式：{"ts":"<ts>","level":"<level>","msg":"<msg>","att":{"service":"<svc>","code":"<code>"},"_slot":"<N>"}
var fixed5Tpl = [5]struct{ level, msg, svc, code string }{
	{"info", "Request completed successfully", "api-gateway", "200"},
	{"warn", "Upstream timeout after 30s waiting for response", "payment-svc", "503"},
	{"error", "Token validation failed: signature mismatch", "auth-service", "401"},
	{"debug", "Health check passed", "notify-svc", "200"},
	{"info", "Database query executed in 3ms", "order-svc", "200"},
}

// fields128Tpl 使用較短但語意相同的訊息與 service，讓完整 RFC3339Nano timestamp、
// level、msg 與巢狀 att 能在 128 B 內共存。msg 末尾補 x 以精確固定行長。
var fields128Tpl = [5]struct{ level, msg, svc, code string }{
	{"info", "request ok", "api", "200"},
	{"warn", "upstream timeout", "pay", "503"},
	{"error", "invalid token", "auth", "401"},
	{"debug", "health ok", "notify", "200"},
	{"info", "query 3ms", "order", "200"},
}

func buildFields128Line(dst []byte, ts string, slot int) []byte {
	t := fields128Tpl[slot]
	dst = dst[:0]
	dst = append(dst, `{"ts":"`...)
	dst = append(dst, ts...)
	dst = append(dst, `","level":"`...)
	dst = append(dst, t.level...)
	dst = append(dst, `","msg":"`...)
	dst = append(dst, t.msg...)

	tailLen := len(`","att":{"service":"`) + len(t.svc) + len(`","code":"`) + len(t.code) + len(`"}}`)
	padLen := 128 - len(dst) - tailLen
	if padLen < 0 {
		padLen = 0
	}
	dst = append(dst, fixedPayloadPad[:padLen]...)
	dst = append(dst, `","att":{"service":"`...)
	dst = append(dst, t.svc...)
	dst = append(dst, `","code":"`...)
	dst = append(dst, t.code...)
	dst = append(dst, `"}}`...)
	return dst
}

func buildFixed5Line(dst []byte, ts string, slot int) []byte {
	t := fixed5Tpl[slot]
	dst = dst[:0]
	dst = append(dst, `{"ts":"`...)
	dst = append(dst, ts...)
	dst = append(dst, `","level":"`...)
	dst = append(dst, t.level...)
	dst = append(dst, `","msg":"`...)
	dst = append(dst, t.msg...)
	dst = append(dst, `","att":{"service":"`...)
	dst = append(dst, t.svc...)
	dst = append(dst, `","code":"`...)
	dst = append(dst, t.code...)
	dst = append(dst, `"},"_slot":"`...)
	dst = append(dst, byte('1'+slot))
	dst = append(dst, '"', '}')
	return dst
}

// buildFixedFewFieldsLine維持json-fixed5的5個top-level欄位，只延長既有msg值。
// 不新增payload欄位，讓相同大小下的少欄／多欄比較只改變欄位結構。
func buildFixedFewFieldsLine(dst []byte, ts string, slot, targetLen int) []byte {
	t := fixed5Tpl[slot]
	dst = dst[:0]
	dst = append(dst, `{"ts":"`...)
	dst = append(dst, ts...)
	dst = append(dst, `","level":"`...)
	dst = append(dst, t.level...)
	dst = append(dst, `","msg":"`...)
	dst = append(dst, t.msg...)

	// 先計算其餘固定結構長度，剩餘空間全部加在既有msg字串內。
	tailLen := len(`","att":{"service":"`) + len(t.svc) + len(`","code":"`) +
		len(t.code) + len(`"},"_slot":"1"}`)
	padLen := targetLen - len(dst) - tailLen
	if padLen < 0 {
		padLen = 0
	}
	dst = append(dst, fixedPayloadPad[:padLen]...)
	dst = append(dst, `","att":{"service":"`...)
	dst = append(dst, t.svc...)
	dst = append(dst, `","code":"`...)
	dst = append(dst, t.code...)
	dst = append(dst, `"},"_slot":"`...)
	dst = append(dst, byte('1'+slot))
	dst = append(dst, '"', '}')
	return dst
}

func buildFixed256Line(dst []byte, ts string, slot int) []byte {
	return buildFixedFewFieldsLine(dst, ts, slot, 256)
}

func buildFixed512Line(dst []byte, ts string, slot int) []byte {
	return buildFixedFewFieldsLine(dst, ts, slot, 512)
}

// 與 fixed5 使用相同 level/message/attribute 分布，只增加固定 payload。
// 每筆 JSON（不含 TCP 換行）精確為 targetLen bytes。
var fixedPayloadPad = strings.Repeat("x", 2048)

func buildFixedSizeLine(dst []byte, ts string, slot, targetLen int) []byte {
	dst = buildFixed5Line(dst, ts, slot)
	dst = dst[:len(dst)-1]
	dst = append(dst, `,"payload":"`...)
	padLen := targetLen - len(dst) - 2 // closing quote + }
	if padLen < 0 {
		padLen = 0
	}
	dst = append(dst, fixedPayloadPad[:padLen]...)
	dst = append(dst, '"', '}')
	return dst
}

func buildFixed1KLine(dst []byte, ts string, slot int) []byte {
	return buildFixedSizeLine(dst, ts, slot, 1024)
}

// fields1K 是常見服務／HTTP／追蹤／部署 metadata。這個 workload 刻意以許多
// 獨立欄位達到 1 KiB，而不是用單一 payload 放大字串，藉此量測每欄位的
// parse、flatten、配置與 encode 成本。
var fields1K = [...]struct{ key, value string }{
	{"region", "ap-southeast-1"},
	{"zone", "ap-southeast-1a"},
	{"environment", "production"},
	{"hostname", "worker-node-17"},
	{"namespace", "payments"},
	{"pod_name", "payment-api-7f84"},
	{"container_name", "payment-api"},
	{"cluster", "prod-taipei-01"},
	{"version", "2026.07.31"},
	{"trace_id", "4bf92f3577b34da6"},
	{"span_id", "00f067aa0ba902b7"},
	{"request_id", "req-01J4M7V8K2"},
	{"method", "POST"},
	{"path", "/api/v2/orders"},
	{"protocol", "HTTP/1.1"},
	{"status_code", "503"},
	{"latency_ms", "1240"},
	{"client_ip", "192.168.10.42"},
	{"user_agent", "checkout-service"},
	{"tenant_id", "tenant-1042"},
	{"account_id", "acct-728194"},
	{"operation", "create-order"},
	{"component", "http-handler"},
	{"datacenter", "tpe-dc-02"},
	{"release", "payment-api-r42"},
	{"team", "platform-observe"},
}

func appendJSONStringField(dst []byte, key, value string) []byte {
	dst = append(dst, ',', '"')
	dst = append(dst, key...)
	dst = append(dst, `":"`...)
	dst = append(dst, value...)
	dst = append(dst, '"')
	return dst
}

func buildMultiFieldsLine(dst []byte, ts string, slot, targetLen, metadataFields int) []byte {
	dst = buildFixed5Line(dst, ts, slot)
	dst = dst[:len(dst)-1] // 移除根物件的 closing brace

	for _, f := range fields1K[:metadataFields] {
		dst = appendJSONStringField(dst, f.key, f.value)
	}

	// 用多個 detail_NN 欄位平均分配剩餘 bytes；每個 value 最多 32 bytes，
	// 避免退化成另一個大型 payload 欄位。
	remaining := targetLen - len(dst) - 1 // 保留最後的 }
	const fieldOverhead = 15              // ,"detail_00":""
	fieldCount := (remaining + fieldOverhead + 31) / (fieldOverhead + 32)
	if fieldCount < 1 {
		fieldCount = 1
	}
	valueBytes := remaining - fieldCount*fieldOverhead
	for i := 0; i < fieldCount; i++ {
		n := valueBytes / (fieldCount - i)
		if n > 32 {
			n = 32
		}
		dst = fmt.Appendf(dst, `,"detail_%02d":"`, i)
		dst = append(dst, fixedPayloadPad[:n]...)
		dst = append(dst, '"')
		valueBytes -= n
	}
	dst = append(dst, '}')
	return dst
}

func buildFields512Line(dst []byte, ts string, slot int) []byte {
	// 5 個基礎欄位 + 11 個 metadata + 1–2 個 detail = 17–18 個 top-level 欄位。
	return buildMultiFieldsLine(dst, ts, slot, 512, 11)
}

func buildFields256Line(dst []byte, ts string, slot int) []byte {
	// 5 個基礎欄位 + 2 個 metadata + 1–2 個 detail = 8–9 個 top-level 欄位。
	return buildMultiFieldsLine(dst, ts, slot, 256, 2)
}

func buildFields1KLine(dst []byte, ts string, slot int) []byte {
	return buildMultiFieldsLine(dst, ts, slot, 1024, len(fields1K))
}

// buildExactFieldCountLine 維持既有 5 個基礎 top-level 欄位及 26 個 metadata
// 欄位，再以 detail_NN 補到指定欄位數；剩餘 bytes 平均分散到 detail value。
// 這讓 1536/2048 B workload 同時固定總大小與欄位數。
func buildExactFieldCountLine(dst []byte, ts string, slot, targetLen, totalFields, metadataFields int) []byte {
	dst = buildFixed5Line(dst, ts, slot)
	dst = dst[:len(dst)-1]

	for _, f := range fields1K[:metadataFields] {
		dst = appendJSONStringField(dst, f.key, f.value)
	}

	const baseFields = 5
	const fieldOverhead = 15 // ,"detail_00":""
	detailCount := totalFields - baseFields - metadataFields
	valueBytes := targetLen - len(dst) - 1 - detailCount*fieldOverhead
	for i := 0; i < detailCount; i++ {
		n := valueBytes / (detailCount - i)
		dst = fmt.Appendf(dst, `,"detail_%02d":"`, i)
		dst = append(dst, fixedPayloadPad[:n]...)
		dst = append(dst, '"')
		valueBytes -= n
	}
	dst = append(dst, '}')
	return dst
}

func buildFields768Line(dst []byte, ts string, slot int) []byte {
	// 5 個基礎欄位 + 20 個 metadata + 2 個 detail = 27 個 top-level 欄位。
	return buildExactFieldCountLine(dst, ts, slot, 768, 27, 20)
}

func buildFields1536Line(dst []byte, ts string, slot int) []byte {
	return buildExactFieldCountLine(dst, ts, slot, 1536, 54, len(fields1K))
}

func buildFields2KLine(dst []byte, ts string, slot int) []byte {
	return buildExactFieldCountLine(dst, ts, slot, 2048, 72, len(fields1K))
}

func buildFixed2KLine(dst []byte, ts string, slot int) []byte {
	return buildFixedSizeLine(dst, ts, slot, 2048)
}

// -----------------------------------------------------------------
// Helper functions
// -----------------------------------------------------------------

func appendLogfmtValue(dst []byte, v string) []byte {
	needQuote := false
	for i := 0; i < len(v); i++ {
		switch v[i] {
		case ' ', '\t', '\n', '\r', '"', '=':
			needQuote = true
		}
		if needQuote {
			break
		}
	}

	if !needQuote {
		return append(dst, v...)
	}

	return strconv.AppendQuote(dst, v)
}

func levelToSyslogSeverity(level string) int {
	switch level {
	case "debug":
		return 7
	case "info":
		return 6
	case "warn":
		return 4
	case "error":
		return 3
	default:
		return 6
	}
}

// -----------------------------------------------------------------
// Line builders - logfmt
// -----------------------------------------------------------------

func buildSimpleLogfmtLine(dst []byte, ts, level, svc, code string) []byte {
	dst = dst[:0]

	dst = append(dst, "ts="...)
	dst = appendLogfmtValue(dst, ts)

	dst = append(dst, " level="...)
	dst = appendLogfmtValue(dst, level)

	dst = append(dst, " msg="...)
	dst = appendLogfmtValue(dst, simpleMessages[rand.Intn(len(simpleMessages))])

	dst = append(dst, " service="...)
	dst = appendLogfmtValue(dst, svc)

	dst = append(dst, " code="...)
	dst = appendLogfmtValue(dst, code)

	return dst
}

func buildComplexLogfmtLine(dst []byte, ts, level, svc, code, region, traceID string, latencyMs int) []byte {
	dst = dst[:0]

	dst = append(dst, "ts="...)
	dst = appendLogfmtValue(dst, ts)

	dst = append(dst, " level="...)
	dst = appendLogfmtValue(dst, level)

	dst = append(dst, " msg="...)
	dst = appendLogfmtValue(dst, complexMessages[rand.Intn(len(complexMessages))])

	dst = append(dst, " service="...)
	dst = appendLogfmtValue(dst, svc)

	dst = append(dst, " code="...)
	dst = appendLogfmtValue(dst, code)

	dst = append(dst, " region="...)
	dst = appendLogfmtValue(dst, region)

	dst = append(dst, " trace_id="...)
	dst = appendLogfmtValue(dst, traceID)

	dst = append(dst, " latency_ms="...)
	dst = strconv.AppendInt(dst, int64(latencyMs), 10)

	dst = append(dst, " host="...)
	dst = fmt.Appendf(dst, "worker-%02d", rand.Intn(32))

	dst = append(dst, " env=production"...)

	return dst
}

// -----------------------------------------------------------------
// Line builders - syslog (RFC5424-like)
// -----------------------------------------------------------------

func buildSimpleSyslogLine(dst []byte, ts, level, svc, code string) []byte {
	dst = dst[:0]

	severity := levelToSyslogSeverity(level)
	priority := 8 + severity // facility=1(user-level)
	host := fmt.Sprintf("worker-%02d", rand.Intn(32))
	msg := simpleMessages[rand.Intn(len(simpleMessages))]

	dst = append(dst, '<')
	dst = strconv.AppendInt(dst, int64(priority), 10)
	dst = append(dst, '>', '1', ' ')

	dst = append(dst, ts...)
	dst = append(dst, ' ')

	dst = append(dst, host...)
	dst = append(dst, ' ')

	dst = append(dst, svc...)
	dst = append(dst, ' ')

	dst = append(dst, '-', ' ', '-', ' ', '-', ' ')

	dst = append(dst, msg...)
	dst = append(dst, ' ')

	dst = append(dst, "level="...)
	dst = appendLogfmtValue(dst, level)

	dst = append(dst, " code="...)
	dst = appendLogfmtValue(dst, code)

	dst = append(dst, " service="...)
	dst = appendLogfmtValue(dst, svc)

	return dst
}

func buildComplexSyslogLine(dst []byte, ts, level, svc, code, region, traceID string, latencyMs int) []byte {
	dst = dst[:0]

	severity := levelToSyslogSeverity(level)
	priority := 8 + severity // facility=1(user-level)
	host := fmt.Sprintf("worker-%02d", rand.Intn(32))
	msg := complexMessages[rand.Intn(len(complexMessages))]

	dst = append(dst, '<')
	dst = strconv.AppendInt(dst, int64(priority), 10)
	dst = append(dst, '>', '1', ' ')

	dst = append(dst, ts...)
	dst = append(dst, ' ')

	dst = append(dst, host...)
	dst = append(dst, ' ')

	dst = append(dst, svc...)
	dst = append(dst, ' ')

	dst = append(dst, '-', ' ', '-', ' ', '-', ' ')

	dst = append(dst, msg...)
	dst = append(dst, ' ')

	dst = append(dst, "level="...)
	dst = appendLogfmtValue(dst, level)

	dst = append(dst, " code="...)
	dst = appendLogfmtValue(dst, code)

	dst = append(dst, " region="...)
	dst = appendLogfmtValue(dst, region)

	dst = append(dst, " trace_id="...)
	dst = appendLogfmtValue(dst, traceID)

	dst = append(dst, " latency_ms="...)
	dst = strconv.AppendInt(dst, int64(latencyMs), 10)

	dst = append(dst, " host="...)
	dst = appendLogfmtValue(dst, host)

	dst = append(dst, " env=production"...)

	return dst
}

// invalidLines covers several failure scenarios for the parser
var invalidLines = []string{
	`not json at all`,
	`{"ts":"2024-01-01","broken":`,
	``,
	`   `,
	`{"ts":null,"level":123,"msg":true}`,
	strings.Repeat("a", 8192), // 超長行
}

func pickInvalidLine() []byte {
	return []byte(invalidLines[rand.Intn(len(invalidLines))])
}

// [pattern_gaps G5] -malformed-rate：以 seq 的乘法雜湊決定（與 test/compare/correctness/generator 相同的雜湊與 1024 B 樣式），
// 被選中的行整行換成無法解析的 1024 B 內容；原本的 slot（level）因此消失。
var malformedPerMillion int64

func isMalformedSeq(seq uint64) bool {
	h := (seq + 1) * 0x9E3779B97F4A7C15
	h ^= h >> 31
	return int64(h%1000000) < malformedPerMillion
}

func malformedLine1K(dst []byte, seq uint64) []byte {
	patterns := []string{"not-json-", "{", `{"ts":`, `{"unterminated":"`, `[] trailing`}
	p := patterns[(seq/20)%uint64(len(patterns))]
	dst = append(dst[:0], p...)
	fill := byte(' ')
	if strings.HasPrefix(p, "not-json") || strings.Contains(p, "unterminated") {
		fill = 'x'
	}
	for len(dst) < 1024 {
		dst = append(dst, fill)
	}
	return dst[:1024]
}

// -----------------------------------------------------------------
// send_lag 統計（ts-mode=sched 時有效）
//
// send_lag = 實際送出時刻 − 排程時刻。
// 無排隊時應貼近 0；DUT backpressure 卡住 gen 時會變大。
// 量測結果的有效性檢查：p99 send_lag 必須遠小於待測延遲的量級。
// -----------------------------------------------------------------

var lagBoundsMs = [...]float64{1, 5, 10, 50, 100, 500, 1000}

type lagStats struct {
	counts [len(lagBoundsMs) + 1]uint64 // ≤1ms, ≤5, ≤10, ≤50, ≤100, ≤500, ≤1000, >1000
	total  uint64
	maxMs  float64
}

func (l *lagStats) record(d time.Duration) {
	ms := float64(d) / float64(time.Millisecond)
	if ms < 0 {
		ms = 0
	}
	if ms > l.maxMs {
		l.maxMs = ms
	}
	l.total++
	for i, b := range lagBoundsMs {
		if ms <= b {
			l.counts[i]++
			return
		}
	}
	l.counts[len(lagBoundsMs)]++
}

// quantileUpperBoundMs 回傳保守估計：q 分位數所落在的 bucket 上界（ms）。
func (l *lagStats) quantileUpperBoundMs(q float64) float64 {
	if l.total == 0 {
		return 0
	}
	need := uint64(math.Ceil(q * float64(l.total)))
	var cum uint64
	for i, c := range l.counts {
		cum += c
		if cum >= need {
			if i < len(lagBoundsMs) {
				return lagBoundsMs[i]
			}
			return l.maxMs
		}
	}
	return l.maxMs
}

func (l *lagStats) summary() string {
	if l.total == 0 {
		return "send_lag: no samples"
	}
	var sb strings.Builder
	fmt.Fprintf(&sb, "send_lag: n=%d p99<=%.0fms p999<=%.0fms max=%.1fms hist=[",
		l.total, l.quantileUpperBoundMs(0.99), l.quantileUpperBoundMs(0.999), l.maxMs)
	var cum uint64
	for i, c := range l.counts {
		cum += c
		label := ">1000ms"
		if i < len(lagBoundsMs) {
			label = fmt.Sprintf("<=%.0fms", lagBoundsMs[i])
		}
		fmt.Fprintf(&sb, " %s:%.3f%%", label, 100*float64(cum)/float64(l.total))
	}
	sb.WriteString(" ]")
	return sb.String()
}

// -----------------------------------------------------------------
// lineOut — 輸出目標抽象
//
//   writeLine(b)  寫一行（不含換行，內部自行補 \n）
//   flush()       強制排清（用於 LOOP trigger 後的即時發送）
//   tick(now)     週期性排清檢查（每個外層迴圈迭代呼叫一次）
//   close()       最終排清並關閉
// -----------------------------------------------------------------

type lineOut interface {
	writeLine(b []byte) error
	flush() error
	tick(now time.Time) error
	close() error
}

// ── stdoutOut：原有行為（buffered stdout + 週期排清）────────────────────────

type stdoutOut struct {
	w          *bufio.Writer
	flushEvery time.Duration
	lastFlush  time.Time
}

func (s *stdoutOut) writeLine(b []byte) error {
	if _, err := s.w.Write(b); err != nil {
		return err
	}
	return s.w.WriteByte('\n')
}

func (s *stdoutOut) flush() error { return s.w.Flush() }

func (s *stdoutOut) tick(now time.Time) error {
	if now.Sub(s.lastFlush) >= s.flushEvery {
		s.lastFlush = now
		return s.w.Flush()
	}
	return nil
}

func (s *stdoutOut) close() error { return s.w.Flush() }

// ── tcpLineOut：每產生一行立刻送出（send-unit=line）─────────────────────────

type tcpLineOut struct {
	conn net.Conn
	buf  []byte
}

func (t *tcpLineOut) writeLine(b []byte) error {
	t.buf = append(t.buf[:0], b...)
	t.buf = append(t.buf, '\n')
	_, err := t.conn.Write(t.buf)
	return err
}

func (t *tcpLineOut) flush() error           { return nil }
func (t *tcpLineOut) tick(_ time.Time) error { return nil }
func (t *tcpLineOut) close() error           { return t.conn.Close() }

// ── tcpBatchOut：累積 N 行再送出（send-unit=batch）──────────────────────────

type tcpBatchOut struct {
	conn       net.Conn
	w          *bufio.Writer
	batchLines int
	count      int
	flushEvery time.Duration
	lastFlush  time.Time
}

func (t *tcpBatchOut) writeLine(b []byte) error {
	if _, err := t.w.Write(b); err != nil {
		return err
	}
	if err := t.w.WriteByte('\n'); err != nil {
		return err
	}
	t.count++
	if t.count >= t.batchLines {
		t.count = 0
		return t.w.Flush()
	}
	return nil
}

func (t *tcpBatchOut) flush() error {
	t.count = 0
	return t.w.Flush()
}

func (t *tcpBatchOut) tick(now time.Time) error {
	if now.Sub(t.lastFlush) >= t.flushEvery {
		t.lastFlush = now
		return t.flush()
	}
	return nil
}

func (t *tcpBatchOut) close() error {
	if err := t.w.Flush(); err != nil {
		return err
	}
	return t.conn.Close()
}

// ── multiOut：多條 TCP 連線 round-robin（-conns > 1）────────────────────────

type multiOut struct {
	outs []lineOut
	next int
}

func (m *multiOut) writeLine(b []byte) error {
	o := m.outs[m.next]
	m.next = (m.next + 1) % len(m.outs)
	return o.writeLine(b)
}

func (m *multiOut) flush() error {
	for _, o := range m.outs {
		if err := o.flush(); err != nil {
			return err
		}
	}
	return nil
}

func (m *multiOut) tick(now time.Time) error {
	for _, o := range m.outs {
		if err := o.tick(now); err != nil {
			return err
		}
	}
	return nil
}

func (m *multiOut) close() error {
	var firstErr error
	for _, o := range m.outs {
		if err := o.close(); err != nil && firstErr == nil {
			firstErr = err
		}
	}
	return firstErr
}

// -----------------------------------------------------------------
// Main loop
// -----------------------------------------------------------------

func main() {
	flag.Parse()

	if *seed != 0 {
		rand.Seed(*seed)
	} else {
		rand.Seed(time.Now().UnixNano())
	}

	if *rate <= 0 {
		fmt.Fprintln(os.Stderr, "rate must be > 0")
		os.Exit(1)
	}

	// Open diagnostic log output.
	var logOut *os.File
	if *logFile == "-" {
		logOut = os.Stderr
	} else {
		var err error
		logOut, err = os.OpenFile(*logFile, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0644)
		if err != nil {
			fmt.Fprintf(os.Stderr, "cannot open log file %s: %v\n", *logFile, err)
			os.Exit(1)
		}
		defer logOut.Close()
	}

	validModes := map[string]bool{
		"json-simple":     true,
		"json-complex":    true,
		"json-mixed":      true,
		"json-fixed5":     true,
		"json-fixed256":   true,
		"json-fixed512":   true,
		"json-fields128":  true,
		"json-fields256":  true,
		"json-fields512":  true,
		"json-fields768":  true,
		"json-fixed1k":    true,
		"json-fields1k":   true,
		"json-fields1536": true,
		"json-fixed2k":    true,
		"json-fields2k":   true,
		"invalid":         true,
		"logfmt-simple":   true,
		"logfmt-complex":  true,
		"logfmt-mixed":    true,
		"syslog-simple":   true,
		"syslog-complex":  true,
		"syslog-mixed":    true,
	}
	if !validModes[*mode] {
		fmt.Fprintln(os.Stderr, "mode must be one of: json-simple, json-complex, json-mixed, json-fixed5, json-fixed256, json-fixed512, json-fields128, json-fields256, json-fields512, json-fields768, json-fixed1k, json-fields1k, json-fields1536, json-fixed2k, json-fields2k, invalid, logfmt-simple, logfmt-complex, logfmt-mixed, syslog-simple, syslog-complex, syslog-mixed")
		os.Exit(1)
	}
	if *trafficShape != "flat" && *trafficShape != "wave" && *trafficShape != "bursty" {
		fmt.Fprintln(os.Stderr, "traffic must be one of: flat, wave, bursty")
		os.Exit(1)
	}
	if *waveAmp < 0 || *waveAmp >= 1.0 {
		fmt.Fprintln(os.Stderr, "wave-amp must be in [0.0, 1.0)")
		os.Exit(1)
	}

	start := time.Now()
	lastReport := start

	// ── 建立輸出目標 ───────────────────────────────────────────────────────────
	var out lineOut
	switch *outputDest {
	case "tcp":
		nConns := *conns
		if nConns < 1 {
			nConns = 1
		}
		outs := make([]lineOut, nConns)
		for i := range outs {
			conn, err := net.Dial("tcp", *tcpAddr)
			if err != nil {
				fmt.Fprintf(os.Stderr, "[gen] cannot connect to %s: %v\n", *tcpAddr, err)
				os.Exit(1)
			}
			if *sendUnit == "batch" {
				outs[i] = &tcpBatchOut{
					conn:       conn,
					w:          bufio.NewWriterSize(conn, *bufferSize),
					batchLines: *batchLines,
					flushEvery: time.Duration(*flushEvery) * time.Millisecond,
					lastFlush:  start,
				}
			} else {
				outs[i] = &tcpLineOut{conn: conn}
			}
		}
		fmt.Fprintf(logOut, "[gen] output=tcp  addr=%s  send-unit=%s  conns=%d", *tcpAddr, *sendUnit, nConns)
		if *sendUnit == "batch" {
			fmt.Fprintf(logOut, "  batch-lines=%d", *batchLines)
		}
		if nConns == 1 {
			out = outs[0]
		} else {
			out = &multiOut{outs: outs}
		}
		fmt.Fprintln(logOut)
	default: // stdout
		fmt.Fprintf(logOut, "[gen] output=stdout\n")
		out = &stdoutOut{
			w:          bufio.NewWriterSize(os.Stdout, *bufferSize),
			flushEvery: time.Duration(*flushEvery) * time.Millisecond,
			lastFlush:  start,
		}
	}

	var total, reportBase uint64
	var malformedTotal, malformedKeepSlot uint64
	if *malformedRate < 0 || *malformedRate > 1 {
		fmt.Fprintln(os.Stderr, "-malformed-rate must be in [0,1]")
		os.Exit(1)
	}
	malformedPerMillion = int64(*malformedRate*1000000 + 0.5)
	var totalBytes uint64
	lineBuf := make([]byte, 0, 512)
	loopSent := false

	// Traffic shaping state
	var cumTarget float64
	lastTick := start
	currentMult := 1.0

	// ts-mode=sched 狀態：本 tick 的 cumTarget 前後值，用於非 flat 波形的排程時間內插
	schedMode := *tsMode == "sched"
	prevCum := 0.0
	tickStart := start
	var lag lagStats

	// Bursty spike state
	inSpike := false
	var spikeEnd time.Time
	var nextSpike time.Time
	if *trafficShape == "bursty" {
		intervalSec := 60.0 / *spikeFreqPM
		// First spike after 0.5–1.5× interval to avoid hitting at t=0
		delay := intervalSec * (0.5 + rand.Float64())
		nextSpike = start.Add(time.Duration(delay * float64(time.Second)))
	}

	fmt.Fprintf(logOut,
		"[gen] start: rate=%d/s duration=%ds mode=%s traffic=%s invalid-rate=%.2f\n",
		*rate, *duration, *mode, *trafficShape, *invalidRate,
	)

	for {
		now := time.Now()

		if *duration > 0 && now.Sub(start) >= time.Duration(*duration)*time.Second {
			break
		}

		// Compute instantaneous rate multiplier and advance the cumulative target.
		{
			tickStart = lastTick
			prevCum = cumTarget
			dt := now.Sub(lastTick).Seconds()
			lastTick = now
			elapsed := now.Sub(start).Seconds()

			switch *trafficShape {
			case "wave":
				// Pure sine wave: integrates to exactly rate*T over full periods.
				currentMult = 1.0 + *waveAmp*math.Sin(2*math.Pi*elapsed / *wavePeriod)
			case "bursty":
				baseMult := 1.0 + *waveAmp*math.Sin(2*math.Pi*elapsed / *wavePeriod)
				// Transition: spike → normal
				if inSpike && now.After(spikeEnd) {
					inSpike = false
					intervalSec := 60.0 / *spikeFreqPM
					jitter := (rand.Float64()*2 - 1) * intervalSec * 0.4
					next := intervalSec + jitter
					if next < 3.0 {
						next = 3.0
					}
					nextSpike = now.Add(time.Duration(next * float64(time.Second)))
				}
				// Transition: normal → spike
				if !inSpike && !nextSpike.IsZero() && !now.Before(nextSpike) {
					inSpike = true
					spikeEnd = now.Add(time.Duration(*spikeDurSec * float64(time.Second)))
				}
				if inSpike {
					currentMult = baseMult * *spikeMult
				} else {
					currentMult = baseMult
				}
			default: // flat
				currentMult = 1.0
			}

			// Clamp to avoid negative rates at extreme amplitudes.
			if currentMult < 0.05 {
				currentMult = 0.05
			}

			cumTarget += float64(*rate) * currentMult * dt
		}

		shouldHaveSent := uint64(cumTarget)

		// 在第 15 秒時注入一筆含 "LOOP" 的觸發 log；訊號寫到 logOut，不污染資料流
		if !loopSent && now.Sub(start) >= 15*time.Second {
			loopSent = true
			fmt.Fprintln(logOut, "[gen-signal] t=15s: about to emit LOOP trigger log")
			ts := now.Format(time.RFC3339Nano)
			loopLog := fmt.Sprintf(`{"ts":"%s","level":"warn","msg":"LOOP trigger","loop":"true"}`, ts)
			if err := out.writeLine([]byte(loopLog)); err != nil {
				fmt.Fprintf(logOut, "write error: %v\n", err)
				os.Exit(1)
			}
			if err := out.flush(); err != nil {
				fmt.Fprintf(logOut, "flush error: %v\n", err)
				os.Exit(1)
			}
			fmt.Fprintf(logOut, "[gen] LOOP trigger emitted at t=%.2fs\n", now.Sub(start).Seconds())
		}

		var produced int
		for total < shouldHaveSent && produced < 50000 {
			// ts-mode=sched：第 n 行（0-based）的排程時刻 = cumTarget 到達 n+1 的時刻。
			// flat 有閉式解；wave/bursty 在本 tick 的 [tickStart, now] 區間線性內插。
			var ts string
			if schedMode {
				var schedAt time.Time
				if *trafficShape == "flat" {
					schedAt = start.Add(time.Duration(float64(total+1) / float64(*rate) * float64(time.Second)))
				} else {
					frac := 1.0
					if cumTarget > prevCum {
						frac = (float64(total+1) - prevCum) / (cumTarget - prevCum)
					}
					if frac < 0 {
						frac = 0
					} else if frac > 1 {
						frac = 1
					}
					schedAt = tickStart.Add(time.Duration(frac * float64(now.Sub(tickStart))))
				}
				if schedAt.After(now) {
					schedAt = now
				}
				ts = schedAt.Format(time.RFC3339Nano)
				lag.record(now.Sub(schedAt))
			} else {
				ts = now.Format(time.RFC3339Nano)
			}
			level := levels[rand.Intn(len(levels))]
			svc := services[rand.Intn(len(services))]
			code := httpCodes[rand.Intn(len(httpCodes))]

			switch *mode {
			case "json-simple":
				lineBuf = buildSimpleJSONLine(lineBuf, ts, level, svc, code)

			case "json-fixed5":
				lineBuf = buildFixed5Line(lineBuf, ts, int(total%5))

			case "json-fixed256":
				lineBuf = buildFixed256Line(lineBuf, ts, int(total%5))

			case "json-fixed512":
				lineBuf = buildFixed512Line(lineBuf, ts, int(total%5))

			case "json-fields128":
				lineBuf = buildFields128Line(lineBuf, ts, int(total%5))

			case "json-fields256":
				lineBuf = buildFields256Line(lineBuf, ts, int(total%5))

			case "json-fields512":
				lineBuf = buildFields512Line(lineBuf, ts, int(total%5))

			case "json-fields768":
				lineBuf = buildFields768Line(lineBuf, ts, int(total%5))

			case "json-fixed1k":
				lineBuf = buildFixed1KLine(lineBuf, ts, int(total%5))

			case "json-fields1k":
				lineBuf = buildFields1KLine(lineBuf, ts, int(total%5))

			case "json-fields1536":
				lineBuf = buildFields1536Line(lineBuf, ts, int(total%5))

			case "json-fixed2k":
				lineBuf = buildFixed2KLine(lineBuf, ts, int(total%5))

			case "json-fields2k":
				lineBuf = buildFields2KLine(lineBuf, ts, int(total%5))

			case "json-complex":
				region := regions[rand.Intn(len(regions))]
				traceID := traceIDs[rand.Intn(len(traceIDs))]
				latency := rand.Intn(2000)
				lineBuf = buildComplexJSONLine(lineBuf, ts, level, svc, code, region, traceID, latency)

			case "json-mixed":
				if rand.Intn(2) == 0 {
					lineBuf = buildSimpleJSONLine(lineBuf, ts, level, svc, code)
				} else {
					region := regions[rand.Intn(len(regions))]
					traceID := traceIDs[rand.Intn(len(traceIDs))]
					latency := rand.Intn(2000)
					lineBuf = buildComplexJSONLine(lineBuf, ts, level, svc, code, region, traceID, latency)
				}

			case "logfmt-simple":
				lineBuf = buildSimpleLogfmtLine(lineBuf, ts, level, svc, code)

			case "logfmt-complex":
				region := regions[rand.Intn(len(regions))]
				traceID := traceIDs[rand.Intn(len(traceIDs))]
				latency := rand.Intn(2000)
				lineBuf = buildComplexLogfmtLine(lineBuf, ts, level, svc, code, region, traceID, latency)

			case "logfmt-mixed":
				if rand.Intn(2) == 0 {
					lineBuf = buildSimpleLogfmtLine(lineBuf, ts, level, svc, code)
				} else {
					region := regions[rand.Intn(len(regions))]
					traceID := traceIDs[rand.Intn(len(traceIDs))]
					latency := rand.Intn(2000)
					lineBuf = buildComplexLogfmtLine(lineBuf, ts, level, svc, code, region, traceID, latency)
				}

			case "syslog-simple":
				lineBuf = buildSimpleSyslogLine(lineBuf, ts, level, svc, code)

			case "syslog-complex":
				region := regions[rand.Intn(len(regions))]
				traceID := traceIDs[rand.Intn(len(traceIDs))]
				latency := rand.Intn(2000)
				lineBuf = buildComplexSyslogLine(lineBuf, ts, level, svc, code, region, traceID, latency)

			case "syslog-mixed":
				if rand.Intn(2) == 0 {
					lineBuf = buildSimpleSyslogLine(lineBuf, ts, level, svc, code)
				} else {
					region := regions[rand.Intn(len(regions))]
					traceID := traceIDs[rand.Intn(len(traceIDs))]
					latency := rand.Intn(2000)
					lineBuf = buildComplexSyslogLine(lineBuf, ts, level, svc, code, region, traceID, latency)
				}

			case "invalid":
				if rand.Float64() < *invalidRate {
					lineBuf = lineBuf[:0]
					lineBuf = append(lineBuf, pickInvalidLine()...)
				} else {
					lineBuf = buildSimpleJSONLine(lineBuf, ts, level, svc, code)
				}
			}

			if malformedPerMillion > 0 && *mode != "invalid" && isMalformedSeq(total) {
				// slot 1／2（warn／error）在 fixed5 系列 mode 會通過 filter；記錄被取代掉的數量供判定修正
				if s := total % 5; s == 1 || s == 2 {
					malformedKeepSlot++
				}
				malformedTotal++
				lineBuf = malformedLine1K(lineBuf, total)
			}

			if err := out.writeLine(lineBuf); err != nil {
				fmt.Fprintf(logOut, "write error: %v\n", err)
				os.Exit(1)
			}

			totalBytes += uint64(len(lineBuf)) + 1
			total++
			produced++
		}

		if err := out.tick(now); err != nil {
			fmt.Fprintf(logOut, "flush error: %v\n", err)
			os.Exit(1)
		}

		if now.Sub(lastReport) >= time.Second {
			windowCount := total - reportBase
			windowSecs := now.Sub(lastReport).Seconds()
			spike := ""
			if inSpike {
				spike = " [SPIKE]"
			}
			fmt.Fprintf(logOut,
				"[gen] total=%d inst=%.0f/s avg=%.0f/s mult=%.2fx%s\n",
				total,
				float64(windowCount)/windowSecs,
				float64(total)/now.Sub(start).Seconds(),
				currentMult,
				spike,
			)
			lastReport = now
			reportBase = total
		}

		if total >= shouldHaveSent {
			time.Sleep(200 * time.Microsecond)
		}
	}

	if err := out.close(); err != nil {
		fmt.Fprintf(logOut, "close error: %v\n", err)
		os.Exit(1)
	}

	elapsed := time.Since(start).Seconds()
	if malformedPerMillion > 0 {
		fmt.Fprintf(logOut, "[gen] malformed: rate=%g total=%d keep_slot=%d\n", *malformedRate, malformedTotal, malformedKeepSlot)
	}
	fmt.Fprintf(logOut,
		"[gen] done: total=%d bytes=%d elapsed=%.3fs avg=%.0f/s avg_mb_s=%.3f\n",
		total, totalBytes, elapsed, float64(total)/elapsed,
		float64(totalBytes)/elapsed/1e6,
	)
	if schedMode {
		fmt.Fprintf(logOut, "[gen] %s\n", lag.summary())
	}
}
