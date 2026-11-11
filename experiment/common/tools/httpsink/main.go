// httpsink — 高流量容量/延遲量測用 HTTP sink（tools/test/server.go 的高流量版）。
//
// 與 server.go 的差異（高流量必要）：
//   - 每秒接收行數/bytes 寫 CSV（容量搜尋的判定依據）
//   - timestamp 以字串掃描抽取（不做整行 JSON unmarshal，sink 不成為瓶頸）
//   - 延遲記錄用緊湊結構（16 bytes/行），千萬行等級不爆記憶體
//   - 延遲解析可用 -parse-ts=false 完全關閉（容量搜尋純計數）
//
// 延遲定義與 server.go 相同：讀完整個 HTTP body 後的 time.Now() − 每行 timestamp。
// 結束時寫出 latency CSV（欄位 ts,receive_unix,latency_ms，與 calc_latency.py 相容；
// ts 欄留空以省記憶體，calc_latency.py 只用後兩欄）。
//
// 用法：httpsink [-port 8080] [-out-dir .] [-parse-ts]
package main

import (
	"bufio"
	"bytes"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"sort"
	"sync"
	"syscall"
	"time"
)

var (
	port     = flag.String("port", "8080", "listen port")
	outDir   = flag.String("out-dir", ".", "output directory for CSVs")
	parseTs  = flag.Bool("parse-ts", true, "parse per-line timestamp and record latency")
	prealloc = flag.Int("prealloc", 0, "preallocate latency record capacity (avoid slice-growth memory spikes on high-volume runs)")
)

// 緊湊延遲記錄：receiveNs 秒級以下由 latUs 反推即可，不存 ts 字串
type latRec struct {
	receiveNs int64
	latUs     int64
}

var (
	mu         sync.Mutex
	totalLines int64
	totalBytes int64
	totalBatch int64
	latBuf     []latRec
	startTime  time.Time
	started    bool
)

// perSecond 統計（獨立 goroutine 每秒快照）
type secStat struct {
	t     float64
	lines int64
	bytes int64
}

var secStats []secStat

// tsKey 掃描：找 "timestamp":"<value>" 的 value 邊界
var tsKey = []byte(`"timestamp":"`)

func extractTs(line []byte) []byte {
	i := bytes.Index(line, tsKey)
	if i < 0 {
		return nil
	}
	rest := line[i+len(tsKey):]
	j := bytes.IndexByte(rest, '"')
	if j < 0 {
		return nil
	}
	return rest[:j]
}

func parseRFC3339Ns(s string) int64 {
	t, err := time.Parse(time.RFC3339Nano, s)
	if err != nil {
		return 0
	}
	return t.UnixNano()
}

func handler(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/ingest" || r.Method != http.MethodPost {
		http.NotFound(w, r)
		return
	}
	body, _ := io.ReadAll(r.Body)
	receiveNs := time.Now().UnixNano()
	bodyLen := int64(len(body))

	nLines := int64(bytes.Count(body, []byte("\n")))
	if len(body) > 0 && body[len(body)-1] != '\n' {
		nLines++
	}

	var recs []latRec
	if *parseTs {
		recs = make([]latRec, 0, nLines)
		for len(body) > 0 {
			nl := bytes.IndexByte(body, '\n')
			var line []byte
			if nl < 0 {
				line, body = body, nil
			} else {
				line, body = body[:nl], body[nl+1:]
			}
			if len(line) == 0 {
				continue
			}
			tsb := extractTs(line)
			if tsb == nil {
				continue
			}
			tsNs := parseRFC3339Ns(string(tsb))
			if tsNs == 0 {
				continue
			}
			recs = append(recs, latRec{receiveNs: receiveNs, latUs: (receiveNs - tsNs) / 1e3})
		}
	}

	mu.Lock()
	if !started {
		started = true
		startTime = time.Now()
	}
	totalBatch++
	totalLines += nLines
	totalBytes += bodyLen
	mu.Unlock()

	w.WriteHeader(http.StatusOK)
	_, _ = w.Write([]byte(`{"status":"ok"}`))

	if len(recs) > 0 {
		mu.Lock()
		latBuf = append(latBuf, recs...)
		mu.Unlock()
	}
}

func shutdown() {
	mu.Lock()
	defer mu.Unlock()

	// 每秒統計 CSV
	f, err := os.Create(*outDir + "/recv_per_sec.csv")
	if err == nil {
		w := bufio.NewWriter(f)
		fmt.Fprintln(w, "t,lines,bytes")
		for _, s := range secStats {
			fmt.Fprintf(w, "%.0f,%d,%d\n", s.t, s.lines, s.bytes)
		}
		w.Flush()
		f.Close()
	}

	elapsed := 0.0
	if started {
		elapsed = time.Since(startTime).Seconds()
	}
	fmt.Printf("[httpsink] done: batches=%d lines=%d bytes=%d elapsed=%.1fs avg=%.0f/s avg_mb_s=%.3f\n",
		totalBatch, totalLines, totalBytes, elapsed,
		float64(totalLines)/max(elapsed, 0.001), float64(totalBytes)/max(elapsed, 0.001)/1e6)

	// 延遲 CSV（與 calc_latency.py 相容）
	if *parseTs && len(latBuf) > 0 {
		f, err := os.Create(*outDir + "/latency_worker_1.csv")
		if err != nil {
			fmt.Fprintf(os.Stderr, "[httpsink] create latency csv: %v\n", err)
			return
		}
		w := bufio.NewWriterSize(f, 1<<20)
		fmt.Fprintln(w, "ts,receive_unix,latency_ms")
		for _, r := range latBuf {
			fmt.Fprintf(w, ",%.6f,%.3f\n", float64(r.receiveNs)/1e9, float64(r.latUs)/1e3)
		}
		w.Flush()
		f.Close()
		fmt.Printf("[httpsink] latency csv: %d records\n", len(latBuf))

		// 大量記錄時跳過內部統計(排序副本會多吃數百 MB;統計交給 calc_latency_stream.py)
		if len(latBuf) > 5_000_000 {
			return
		}
		lats := make([]float64, len(latBuf))
		for i, r := range latBuf {
			lats[i] = float64(r.latUs) / 1e3
		}
		sort.Float64s(lats)
		p := func(q float64) float64 {
			k := q / 100 * float64(len(lats)-1)
			lo := int(k)
			hi := lo + 1
			if hi >= len(lats) {
				hi = len(lats) - 1
			}
			return lats[lo] + (k-float64(lo))*(lats[hi]-lats[lo])
		}
		fmt.Printf("[httpsink] all-run lat: p50=%.1fms p99=%.1fms max=%.1fms\n", p(50), p(99), lats[len(lats)-1])
	}
}

func main() {
	flag.Parse()
	if *parseTs && *prealloc > 0 {
		latBuf = make([]latRec, 0, *prealloc)
	}

	// 每秒快照
	go func() {
		start := time.Now()
		var prevL, prevB int64
		tick := time.NewTicker(time.Second)
		for range tick.C {
			mu.Lock()
			l, b := totalLines, totalBytes
			mu.Unlock()
			s := secStat{t: time.Since(start).Seconds(), lines: l - prevL, bytes: b - prevB}
			prevL, prevB = l, b
			mu.Lock()
			secStats = append(secStats, s)
			mu.Unlock()
			fmt.Printf("[httpsink] t=%.0fs lines/s=%d MB/s=%.2f total=%d\n", s.t, s.lines, float64(s.bytes)/1e6, l)
		}
	}()

	srv := &http.Server{Addr: "0.0.0.0:" + *port, Handler: http.HandlerFunc(handler)}
	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			fmt.Fprintf(os.Stderr, "[httpsink] serve: %v\n", err)
			os.Exit(1)
		}
	}()
	fmt.Printf("[httpsink] listening :%s parse-ts=%v out-dir=%s\n", *port, *parseTs, *outDir)

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	<-sig
	shutdown()
}
