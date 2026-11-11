// Format：每筆輸出一個 json-flat 物件 {"id":..,"timestamp":..,"level":..,"message":..,<tag>:<value>...}，
// 以 [4-byte little-endian 長度][資料] 的 frame 串接。輸出與 Rust 版 format 的 json-flat 相同。
using System.Buffers.Binary;
using System.Text;
using PP = FormatPluginWorld.wit.Imports.local.logProcess.v0_3_0.IPipelineProcessImports;

namespace FormatPluginWorld;

public class FormatPluginWorldExportsImpl : IFormatPluginWorldExports
{
    private static readonly string[] LevelNames = ["debug", "info", "warn", "error", "crit", "alert", "emerg"];

    public static byte[] Format(List<PP.LogEntry> structData)
    {
        var output = new MemoryStream();
        var entry = new StringBuilder();
        Span<byte> header = stackalloc byte[4];
        foreach (var e in structData)
        {
            entry.Clear();
            entry.Append("{\"id\":").Append(e.id);
            entry.Append(",\"timestamp\":");
            AppendJsonString(entry, e.timestamp);
            entry.Append(",\"level\":\"").Append(LevelName(e.level)).Append('"');
            entry.Append(",\"message\":");
            AppendJsonString(entry, e.message);
            foreach (var (key, value) in e.tags)
            {
                entry.Append(',');
                AppendJsonString(entry, key);
                entry.Append(':');
                AppendJsonString(entry, value);
            }
            entry.Append('}');

            var bytes = Encoding.UTF8.GetBytes(entry.ToString());
            BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)bytes.Length);
            output.Write(header);
            output.Write(bytes);
        }
        return output.ToArray();
    }

    public static ulong ReportUsage() => 0;

    private static string LevelName(PP.LogLevel level) =>
        (int)level < LevelNames.Length ? LevelNames[(int)level] : "unknown";

    private static void AppendJsonString(StringBuilder sb, string s)
    {
        sb.Append('"');
        foreach (var c in s)
        {
            switch (c)
            {
                case '"': sb.Append("\\\""); break;
                case '\\': sb.Append("\\\\"); break;
                case '\n': sb.Append("\\n"); break;
                case '\r': sb.Append("\\r"); break;
                case '\t': sb.Append("\\t"); break;
                default:
                    if (c < 0x20)
                    {
                        sb.Append("\\u").Append(((int)c).ToString("x4"));
                    }
                    else
                    {
                        sb.Append(c);
                    }
                    break;
            }
        }
        sb.Append('"');
    }
}
