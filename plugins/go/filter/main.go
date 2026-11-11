// Filter：保留 level >= warn 的日誌。
package main

import (
	pipelineprocess "wcm/plugins/internal/local/log-process/pipeline-process"
	reductionplugin "wcm/plugins/internal/local/log-process/reduction-plugin"
	world "wcm/plugins/internal/local/log-process/reduction-plugin-go"

	"go.bytecodealliance.org/cm"
)

type filterResult = cm.Result[world.PluginErrorShape, cm.List[reductionplugin.FilterResult], reductionplugin.PluginError]

func init() {
	world.Exports.Filter = filter
	world.Exports.ReportUsage = func() uint64 { return 0 }
}

func filter(entries cm.List[reductionplugin.LogEntry]) filterResult {
	items := entries.Slice()
	results := make([]reductionplugin.FilterResult, len(items))
	for i, entry := range items {
		results[i] = reductionplugin.FilterResult{ID: entry.ID, Keep: entry.Level >= pipelineprocess.LogLevelWarn}
	}
	return cm.OK[filterResult](cm.ToList(results))
}

func main() {}
