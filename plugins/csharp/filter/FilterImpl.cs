// Filter：保留 level >= warn 的日誌。
using PP = ReductionPluginWorld.wit.Imports.local.logProcess.v0_3_0.IPipelineProcessImports;

namespace ReductionPluginWorld;

public class ReductionPluginWorldExportsImpl : IReductionPluginWorldExports
{
    public static List<PP.FilterResult> Filter(List<PP.LogEntry> entries)
    {
        var results = new List<PP.FilterResult>(entries.Count);
        foreach (var entry in entries)
        {
            results.Add(new PP.FilterResult(entry.id, entry.level >= PP.LogLevel.WARN));
        }
        return results;
    }

    public static ulong ReportUsage() => 0;
}
