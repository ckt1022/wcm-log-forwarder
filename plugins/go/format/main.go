// Format：每筆輸出一個 json-flat 物件 {"id":..,"timestamp":..,"level":..,"message":..,<tag>:<value>...}，
// 以 [4-byte little-endian 長度][資料] 的 frame 串接。輸出與 Rust 版 format 的 json-flat 相同。
package main

import (
	"encoding/binary"
	"strconv"

	formatplugin "wcm/plugins/internal/local/log-process/format-plugin"
	world "wcm/plugins/internal/local/log-process/format-plugin-go"

	"go.bytecodealliance.org/cm"
)

type formatResult = cm.Result[world.PluginErrorShape, cm.List[uint8], formatplugin.PluginError]

var levelNames = []string{"debug", "info", "warn", "error", "crit", "alert", "emerg"}

func init() {
	world.Exports.Format = format
	world.Exports.ReportUsage = func() uint64 { return 0 }
}

func format(structData cm.List[formatplugin.LogEntry]) formatResult {
	var out []byte
	var entry []byte
	for _, e := range structData.Slice() {
		entry = entry[:0]
		entry = append(entry, `{"id":`...)
		entry = strconv.AppendUint(entry, e.ID, 10)
		entry = append(entry, `,"timestamp":`...)
		entry = appendJSONString(entry, e.Timestamp)
		entry = append(entry, `,"level":"`...)
		entry = append(entry, levelName(int(e.Level))...)
		entry = append(entry, `","message":`...)
		entry = appendJSONString(entry, e.Message)
		for _, tag := range e.Tags.Slice() {
			entry = append(entry, ',')
			entry = appendJSONString(entry, tag[0])
			entry = append(entry, ':')
			entry = appendJSONString(entry, tag[1])
		}
		entry = append(entry, '}')
		out = binary.LittleEndian.AppendUint32(out, uint32(len(entry)))
		out = append(out, entry...)
	}
	return cm.OK[formatResult](cm.ToList(out))
}

func levelName(level int) string {
	if level < len(levelNames) {
		return levelNames[level]
	}
	return "unknown"
}

func appendJSONString(out []byte, s string) []byte {
	const hex = "0123456789abcdef"
	out = append(out, '"')
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch c {
		case '"':
			out = append(out, '\\', '"')
		case '\\':
			out = append(out, '\\', '\\')
		case '\n':
			out = append(out, '\\', 'n')
		case '\r':
			out = append(out, '\\', 'r')
		case '\t':
			out = append(out, '\\', 't')
		default:
			if c < 0x20 {
				out = append(out, '\\', 'u', '0', '0', hex[c>>4], hex[c&0xf])
			} else {
				out = append(out, c)
			}
		}
	}
	return append(out, '"')
}

func main() {}
