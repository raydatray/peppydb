package gateway

import (
	"net/http"
	"strings"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/collectors"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// latencyBuckets mirrors lib/server/src/metrics.rs: 100 µs up to 10 s.
var latencyBuckets = []float64{
	0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025,
	0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
}

// Metrics holds Prometheus metrics for one running gateway instance.
type Metrics struct {
	registry        *prometheus.Registry
	rpcRequests     *prometheus.CounterVec
	rpcDuration     *prometheus.HistogramVec
	inflight        prometheus.Gauge
	leaderRedirects prometheus.Counter
	readRetries     prometheus.Counter
	leaderProbes    prometheus.Counter
}

// NewMetrics constructs an isolated Prometheus registry with Go runtime,
// process, and gateway RPC metrics.
func NewMetrics() *Metrics {
	reg := prometheus.NewRegistry()
	startedAt := time.Now()

	m := &Metrics{
		registry: reg,
		rpcRequests: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "rpc_requests_total",
			Help:      "Completed gRPC requests by method and status code.",
		}, []string{"method", "code"}),
		rpcDuration: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "rpc_duration_seconds",
			Help:      "End-to-end gateway gRPC handler latency in seconds.",
			Buckets:   latencyBuckets,
		}, []string{"method"}),
		inflight: prometheus.NewGauge(prometheus.GaugeOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "inflight_requests",
			Help:      "Gateway RPCs currently in flight.",
		}),
		leaderRedirects: prometheus.NewCounter(prometheus.CounterOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "leader_redirects_total",
			Help:      "LeaderHint redirects followed by the gateway.",
		}),
		readRetries: prometheus.NewCounter(prometheus.CounterOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "read_retries_total",
			Help:      "Transient non-hint read retries performed by the gateway.",
		}),
		leaderProbes: prometheus.NewCounter(prometheus.CounterOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "leader_probes_total",
			Help:      "Leader discovery probes issued by the gateway.",
		}),
	}

	reg.MustRegister(
		collectors.NewGoCollector(),
		collectors.NewProcessCollector(collectors.ProcessCollectorOpts{}),
		m.rpcRequests,
		m.rpcDuration,
		m.inflight,
		m.leaderRedirects,
		m.readRetries,
		m.leaderProbes,
		prometheus.NewGaugeFunc(prometheus.GaugeOpts{
			Namespace: "peppydb",
			Subsystem: "gateway",
			Name:      "process_uptime_seconds",
			Help:      "Gateway process uptime in seconds.",
		}, func() float64 {
			return time.Since(startedAt).Seconds()
		}),
	)
	return m
}

// Handler returns an HTTP handler serving Prometheus metrics on GET /metrics
// and 404 on all other paths.
func (m *Metrics) Handler() http.Handler {
	promHandler := promhttp.HandlerFor(m.registry, promhttp.HandlerOpts{})
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/metrics" {
			http.NotFound(w, r)
			return
		}
		promHandler.ServeHTTP(w, r)
	})
}

func (m *Metrics) observeRPC(method string, err error, dur time.Duration) {
	if m == nil {
		return
	}
	code := codeLabel(status.Code(err))
	m.rpcRequests.WithLabelValues(method, code).Inc()
	m.rpcDuration.WithLabelValues(method).Observe(dur.Seconds())
}

func (m *Metrics) incInflight() {
	if m != nil {
		m.inflight.Inc()
	}
}

func (m *Metrics) decInflight() {
	if m != nil {
		m.inflight.Dec()
	}
}

// IncLeaderRedirect increments the LeaderHint redirect counter.
func (m *Metrics) IncLeaderRedirect() {
	if m != nil {
		m.leaderRedirects.Inc()
	}
}

// IncReadRetry increments the transient read-retry counter.
func (m *Metrics) IncReadRetry() {
	if m != nil {
		m.readRetries.Inc()
	}
}

// IncLeaderProbe increments the leader discovery probe counter.
func (m *Metrics) IncLeaderProbe() {
	if m != nil {
		m.leaderProbes.Inc()
	}
}

func codeLabel(c codes.Code) string {
	return strings.ToLower(c.String())
}
