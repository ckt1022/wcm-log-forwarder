// Parse：一行 JSON 一筆。ts → timestamp、level → level、msg → message，
// 其餘欄位與 att 物件內的欄位攤平成 tags（依鍵名排序），第一個 tag 為 lang=Go，targettag 為 "A"。
// 任何一行不是合法 JSON 物件時整批回傳 invalid-format，由 host 以二分法找出那一行。
package main

import (
	"encoding/json"
	"sort"
	"strconv"
	"strings"

	parserplugin "wcm/plugins/internal/local/log-process/parser-plugin"
	world "wcm/plugins/internal/local/log-process/parser-plugin-go"
	pipelineprocess "wcm/plugins/internal/local/log-process/pipeline-process"

	"go.bytecodealliance.org/cm"
)

type parseResult = cm.Result[world.ParseErrorShape, cm.List[parserplugin.ParsedEntry], parserplugin.ParseError]

func init() {
	world.Exports.Parse = parse
	world.Exports.ReportUsage = func() uint64 { return 0 }
}

func parse(rawData cm.List[string]) parseResult {
	lines := rawData.Slice()
	entries := make([]parserplugin.ParsedEntry, 0, len(lines))
	for _, line := range lines {
		entry, ok := parseLine(line)
		if !ok {
			return cm.Err[parseResult](pipelineprocess.ParseErrorInvalidFormat("JSON parse error"))
		}
		entries = append(entries, entry)
	}
	return cm.OK[parseResult](cm.ToList(entries))
}

func parseLine(line string) (parserplugin.ParsedEntry, bool) {
	// 整行只解析一次；UseNumber 讓數字保留原始文字
	decoder := json.NewDecoder(strings.NewReader(line))
	decoder.UseNumber()
	var root map[string]any
	if err := decoder.Decode(&root); err != nil || root == nil || decoder.More() {
		return parserplugin.ParsedEntry{}, false
	}
	tags := [][2]string{{"lang", "Go"}}
	tags = appendTags(tags, root, func(key string) bool {
		return key == "ts" || key == "level" || key == "msg" || key == "att"
	})
	if att, ok := root["att"].(map[string]any); ok {
		tags = appendTags(tags, att, func(string) bool { return false })
	}
	return parserplugin.ParsedEntry{
		Timestamp: stringField(root["ts"]),
		Level:     mapLevel(root["level"]),
		Message:   stringField(root["msg"]),
		Tags:      cm.ToList(tags),
		Targettag: "A",
	}, true
}

func appendTags(tags [][2]string, fields map[string]any, skip func(string) bool) [][2]string {
	keys := make([]string, 0, len(fields))
	for key := range fields {
		if !skip(key) {
			keys = append(keys, key)
		}
	}
	sort.Strings(keys)
	for _, key := range keys {
		tags = append(tags, [2]string{key, valueText(fields[key])})
	}
	return tags
}

// 字串直接取值，其他型別輸出 JSON 文字
func valueText(value any) string {
	switch v := value.(type) {
	case string:
		return v
	case json.Number:
		return v.String()
	case bool:
		return strconv.FormatBool(v)
	case nil:
		return "null"
	default:
		text, _ := json.Marshal(v)
		return string(text)
	}
}

func stringField(value any) string {
	s, _ := value.(string)
	return s
}

func mapLevel(value any) pipelineprocess.LogLevel {
	if n, ok := value.(json.Number); ok {
		if i, err := n.Int64(); err == nil && i >= 0 && i <= 6 {
			return pipelineprocess.LogLevel(i)
		}
	}
	switch strings.ToLower(stringField(value)) {
	case "debug":
		return pipelineprocess.LogLevelDebug
	case "warn":
		return pipelineprocess.LogLevelWarn
	case "error":
		return pipelineprocess.LogLevelError
	default:
		return pipelineprocess.LogLevelInfo
	}
}

func main() {}
