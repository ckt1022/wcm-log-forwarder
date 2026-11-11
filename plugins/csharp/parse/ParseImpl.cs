// Parse：一行 JSON 一筆。ts → timestamp、level → level、msg → message，
// 其餘欄位與 att 物件內的欄位攤平成 tags，第一個 tag 為 lang=C#，targettag 為 "A"。
// 任何一行不是合法 JSON 時整批回傳 invalid-format，由 host 以二分法找出那一行。
using System.Text.Json;
using PP = ParserPluginWorld.wit.Imports.local.logProcess.v0_3_0.IPipelineProcessImports;

namespace ParserPluginWorld;

public class ParserPluginWorldExportsImpl : IParserPluginWorldExports
{
    public static List<PP.ParsedEntry> Parse(List<string> rawData)
    {
        var results = new List<PP.ParsedEntry>(rawData.Count);
        foreach (var line in rawData)
        {
            results.Add(ParseLine(line));
        }
        return results;
    }

    public static ulong ReportUsage() => 0;

    private static PP.ParsedEntry ParseLine(string line)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(line);
        }
        catch (JsonException)
        {
            throw new WitException<PP.ParseError>(PP.ParseError.InvalidFormat("JSON parse error"), 0);
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                throw new WitException<PP.ParseError>(PP.ParseError.InvalidFormat("not a JSON object"), 0);
            }

            var tags = new List<(string, string)> { ("lang", "C#") };
            foreach (var property in root.EnumerateObject())
            {
                if (property.Name is not ("ts" or "level" or "msg" or "att"))
                {
                    tags.Add((property.Name, ValueText(property.Value)));
                }
            }
            if (root.TryGetProperty("att", out var att) && att.ValueKind == JsonValueKind.Object)
            {
                foreach (var property in att.EnumerateObject())
                {
                    tags.Add((property.Name, ValueText(property.Value)));
                }
            }

            return new PP.ParsedEntry(
                StringField(root, "ts"),
                MapLevel(root),
                StringField(root, "msg"),
                tags,
                "A");
        }
    }

    private static string StringField(JsonElement root, string name) =>
        root.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? ""
            : "";

    // 字串直接取值，其他型別保留原始的 JSON 文字
    private static string ValueText(JsonElement value) =>
        value.ValueKind == JsonValueKind.String ? value.GetString() ?? "" : value.GetRawText();

    private static PP.LogLevel MapLevel(JsonElement root)
    {
        if (!root.TryGetProperty("level", out var level))
        {
            return PP.LogLevel.INFO;
        }
        if (level.ValueKind == JsonValueKind.Number && level.TryGetInt32(out var n) && n is >= 0 and <= 6)
        {
            return (PP.LogLevel)n;
        }
        return (level.ValueKind == JsonValueKind.String ? level.GetString() : null)?.ToLowerInvariant() switch
        {
            "debug" => PP.LogLevel.DEBUG,
            "warn" => PP.LogLevel.WARN,
            "error" => PP.LogLevel.ERROR,
            _ => PP.LogLevel.INFO,
        };
    }
}
