// Transport：每行加上換行後組成 NDJSON body，以 wasi:http 送出 HTTP POST。
// host 負責 TCP 連線、TLS 與 HTTP framing，也只放行 endpoint map 中的位址。
// 重試由 host 負責；非 2xx 時回報 "server returned HTTP <status>"，host 依狀態碼決定是否重試。
package main

import (
	"strconv"
	"strings"

	pipelineprocess "wcm/plugins/internal/local/log-process/pipeline-process"
	transportplugin "wcm/plugins/internal/local/log-process/transport-plugin"
	world "wcm/plugins/internal/local/log-process/transport-plugin-go"
	transporttypes "wcm/plugins/internal/local/log-process/transport-types"
	outgoinghandler "wcm/plugins/internal/wasi/http/outgoing-handler"
	"wcm/plugins/internal/wasi/http/types"

	"go.bytecodealliance.org/cm"
)

type voidResult = cm.Result[transportplugin.PluginError, struct{}, transportplugin.PluginError]

// body 分段寫入的大小：blocking-write-and-flush 每次最多 4096 位元組
const writeChunk = 4096

type endpoint struct {
	https     bool
	authority string
	path      string
}

var (
	current       *endpoint
	maxBatchBytes uint32
	bytesSent     uint64
)

func init() {
	world.Exports.Init = initTransport
	world.Exports.Transport = transport
	world.Exports.ReportUsage = func() uint64 { return bytesSent }
}

func fail(err pipelineprocess.PluginError) voidResult {
	return cm.Err[voidResult](err)
}

func initTransport(config transporttypes.TransportConfig) voidResult {
	ep, err := parseEndpoint(config.Endpoint)
	if err != "" {
		return fail(pipelineprocess.PluginErrorConfigError(err))
	}
	current = ep
	maxBatchBytes = config.MaxBatchBytes
	return cm.OK[voidResult](struct{}{})
}

func parseEndpoint(url string) (*endpoint, string) {
	var ep endpoint
	switch {
	case strings.HasPrefix(url, "http://"):
		url = url[len("http://"):]
	case strings.HasPrefix(url, "https://"):
		ep.https = true
		url = url[len("https://"):]
	default:
		return nil, "unsupported scheme (only http:// and https:// are supported)"
	}
	ep.authority, ep.path = url, "/"
	if slash := strings.IndexByte(url, '/'); slash >= 0 {
		ep.authority, ep.path = url[:slash], url[slash:]
	}
	if ep.authority == "" {
		return nil, "endpoint is missing a host"
	}
	return &ep, ""
}

func transport(outputData cm.List[cm.List[uint8]]) voidResult {
	if current == nil {
		return fail(pipelineprocess.PluginErrorConfigError("init() not called before transport()"))
	}
	for _, body := range buildBatches(outputData.Slice(), maxBatchBytes) {
		if err := send(current, body); err != "" {
			return fail(pipelineprocess.PluginErrorProcessingFailed(err))
		}
		bytesSent += uint64(len(body))
	}
	return cm.OK[voidResult](struct{}{})
}

// 依 max-batch-bytes 切成多個 body（0 = 不切），每行以換行結尾
func buildBatches(lines []cm.List[uint8], limit uint32) [][]byte {
	var batches [][]byte
	var body []byte
	for _, line := range lines {
		data := line.Slice()
		if limit > 0 && len(body) > 0 && len(body)+len(data)+1 > int(limit) {
			batches = append(batches, body)
			body = nil
		}
		body = append(body, data...)
		body = append(body, '\n')
	}
	if len(body) > 0 {
		batches = append(batches, body)
	}
	return batches
}

func send(ep *endpoint, body []byte) string {
	headers := types.NewFields()
	headers.Append("content-type", types.FieldValue(cm.ToList([]byte("application/octet-stream"))))
	request := types.NewOutgoingRequest(headers)
	request.SetMethod(types.MethodPost())
	if ep.https {
		request.SetScheme(cm.Some(types.SchemeHTTPS()))
	} else {
		request.SetScheme(cm.Some(types.SchemeHTTP()))
	}
	request.SetAuthority(cm.Some(ep.authority))
	request.SetPathWithQuery(cm.Some(ep.path))
	bodyResult := request.Body()
	outgoingBody := *bodyResult.OK()

	// 先送出請求再寫入 body，body 才不受 host 端緩衝大小限制
	handled := outgoinghandler.Handle(request, cm.None[types.RequestOptions]())
	if handled.IsErr() {
		outgoingBody.ResourceDrop()
		return "HTTP request failed (" + handled.Err().String() + ")"
	}
	future := *handled.OK()
	defer future.ResourceDrop()

	streamResult := outgoingBody.Write()
	stream := *streamResult.OK()
	for offset := 0; offset < len(body); offset += writeChunk {
		end := min(offset+writeChunk, len(body))
		written := stream.BlockingWriteAndFlush(cm.ToList(body[offset:end]))
		if written.IsErr() {
			stream.ResourceDrop()
			outgoingBody.ResourceDrop()
			return "body write failed"
		}
	}
	stream.ResourceDrop()
	types.OutgoingBodyFinish(outgoingBody, cm.None[types.Trailers]())

	pollable := future.Subscribe()
	pollable.Block()
	pollable.ResourceDrop()

	outer := future.Get()
	if outer.None() {
		return "HTTP response not ready"
	}
	inner := outer.Some().OK()
	if inner.IsErr() {
		return "HTTP request failed (" + inner.Err().String() + ")"
	}
	response := *inner.OK()
	status := response.Status()
	response.ResourceDrop()
	if status < 200 || status >= 300 {
		// host 依 "HTTP <status>" 判斷是否值得重試
		return "server returned HTTP " + strconv.Itoa(int(status))
	}
	return ""
}

func main() {}
