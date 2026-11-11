// Transport：每行加上換行後組成 NDJSON body，以 wasi:http 送出 HTTP POST。
// host 負責 TCP 連線、TLS 與 HTTP framing，也只放行 endpoint map 中的位址。
// 重試由 host 負責；非 2xx 時回報 "server returned HTTP <status>"，host 依狀態碼決定是否重試。
using PP = TransportPluginWorld.wit.Imports.local.logProcess.v0_3_0.IPipelineProcessImports;
using TT = TransportPluginWorld.wit.Imports.local.logProcess.v0_3_0.ITransportTypesImports;
using Http = TransportPluginWorld.wit.Imports.wasi.http.v0_2_0.ITypesImports;
using Handler = TransportPluginWorld.wit.Imports.wasi.http.v0_2_0.IOutgoingHandlerImports;

namespace TransportPluginWorld;

public class TransportPluginWorldExportsImpl : ITransportPluginWorldExports
{
    // body 分段寫入的大小：blocking-write-and-flush 每次最多 4096 位元組
    private const int WriteChunk = 4096;

    private static Endpoint? _endpoint;
    private static uint _maxBatchBytes;
    private static ulong _bytesSent;

    private sealed record Endpoint(bool Https, string Authority, string Path);

    public static void Init(TT.TransportConfig config)
    {
        _endpoint = ParseEndpoint(config.endpoint);
        _maxBatchBytes = config.maxBatchBytes;
    }

    public static void Transport(List<byte[]> outputData)
    {
        var endpoint = _endpoint ?? throw Error(PP.PluginError.ConfigError("init() not called before transport()"));
        foreach (var body in BuildBatches(outputData, _maxBatchBytes))
        {
            Send(endpoint, body);
            _bytesSent += (ulong)body.Length;
        }
    }

    public static ulong ReportUsage() => _bytesSent;

    private static WitException<PP.PluginError> Error(PP.PluginError error) => new(error, 0);

    private static Endpoint ParseEndpoint(string url)
    {
        bool https;
        string rest;
        if (url.StartsWith("http://", StringComparison.Ordinal))
        {
            https = false;
            rest = url["http://".Length..];
        }
        else if (url.StartsWith("https://", StringComparison.Ordinal))
        {
            https = true;
            rest = url["https://".Length..];
        }
        else
        {
            throw Error(PP.PluginError.ConfigError("unsupported scheme (only http:// and https:// are supported)"));
        }
        var slash = rest.IndexOf('/');
        var authority = slash < 0 ? rest : rest[..slash];
        if (authority.Length == 0)
        {
            throw Error(PP.PluginError.ConfigError("endpoint is missing a host"));
        }
        return new Endpoint(https, authority, slash < 0 ? "/" : rest[slash..]);
    }

    // 依 max-batch-bytes 切成多個 body（0 = 不切），每行以換行結尾
    private static List<byte[]> BuildBatches(List<byte[]> lines, uint maxBatchBytes)
    {
        var batches = new List<byte[]>();
        var current = new MemoryStream();
        foreach (var line in lines)
        {
            var size = line.Length + 1;
            if (maxBatchBytes > 0 && current.Length > 0 && current.Length + size > maxBatchBytes)
            {
                batches.Add(current.ToArray());
                current = new MemoryStream();
            }
            current.Write(line);
            current.WriteByte((byte)'\n');
        }
        if (current.Length > 0)
        {
            batches.Add(current.ToArray());
        }
        return batches;
    }

    private static void Send(Endpoint endpoint, byte[] body)
    {
        var headers = new Http.Fields();
        headers.Append("content-type", "application/octet-stream"u8.ToArray());
        var request = new Http.OutgoingRequest(headers);
        request.SetMethod(Http.Method.Post());
        request.SetScheme(endpoint.Https ? Http.Scheme.Https() : Http.Scheme.Http());
        request.SetAuthority(endpoint.Authority);
        request.SetPathWithQuery(endpoint.Path);
        var outgoingBody = request.Body();

        Http.FutureIncomingResponse future;
        try
        {
            // 先送出請求再寫入 body，body 才不受 host 端緩衝大小限制
            future = Handler.Handle(request, null);
        }
        catch (WitException e)
        {
            outgoingBody.Dispose();
            throw Error(PP.PluginError.ProcessingFailed($"HTTP request failed ({e.Value})"));
        }

        using (var stream = outgoingBody.Write())
        {
            for (var offset = 0; offset < body.Length; offset += WriteChunk)
            {
                stream.BlockingWriteAndFlush(body.AsSpan(offset, Math.Min(WriteChunk, body.Length - offset)));
            }
        }
        Http.OutgoingBody.Finish(outgoingBody, null);

        using (future)
        {
            using (var pollable = future.Subscribe())
            {
                pollable.Block();
            }
            var outer = future.Get() ?? throw Error(PP.PluginError.ProcessingFailed("HTTP response not ready"));
            var inner = outer.AsOk;
            if (inner.IsErr)
            {
                throw Error(PP.PluginError.ProcessingFailed($"HTTP request failed ({inner.AsErr.Tag})"));
            }
            using var response = inner.AsOk;
            var status = response.Status();
            if (status < 200 || status >= 300)
            {
                // host 依 "HTTP <status>" 判斷是否值得重試
                throw Error(PP.PluginError.ProcessingFailed($"server returned HTTP {status}"));
            }
        }
    }
}
