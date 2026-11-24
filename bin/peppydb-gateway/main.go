// peppydb-gateway: stateless gRPC gateway in front of a peppydb cluster (plan 0033).
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"net"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"peppydb/lib/gateway"
)

type options struct {
	listen         string
	backends       string
	metricsListen  string
	requestTimeout time.Duration
	maxRedirects   int
	maxReadRetries int
}

func main() {
	var o options
	flag.StringVar(&o.listen, "listen", "127.0.0.1:7430", "gRPC listen address")
	flag.StringVar(&o.backends, "backends", "", "comma-separated backend peppydb addresses (required)")
	flag.StringVar(&o.metricsListen, "metrics-listen", "", "optional HTTP address serving Prometheus /metrics")
	flag.DurationVar(&o.requestTimeout, "request-timeout", 5*time.Second, "default RPC timeout when caller sets no deadline")
	flag.IntVar(&o.maxRedirects, "max-redirects", 5, "maximum LeaderHint redirects per RPC")
	flag.IntVar(&o.maxReadRetries, "max-read-retries", 5, "maximum non-hint UNAVAILABLE retries for reads")
	flag.Parse()

	if err := run(o); err != nil {
		fmt.Fprintln(os.Stderr, "peppydb-gateway:", err)
		os.Exit(1)
	}
}

func parseBackends(spec string) ([]string, error) {
	var addrs []string
	for part := range strings.SplitSeq(spec, ",") {
		part = strings.TrimSpace(part)
		if part != "" {
			addrs = append(addrs, part)
		}
	}
	if len(addrs) == 0 {
		return nil, errors.New("at least one backend address is required (--backends)")
	}
	return addrs, nil
}

func run(o options) error {
	backends, err := parseBackends(o.backends)
	if err != nil {
		return err
	}
	lis, err := net.Listen("tcp", o.listen)
	if err != nil {
		return fmt.Errorf("listen %s: %w", o.listen, err)
	}
	var metricsLis net.Listener
	if o.metricsListen != "" {
		metricsLis, err = net.Listen("tcp", o.metricsListen)
		if err != nil {
			_ = lis.Close()
			return fmt.Errorf("metrics listen %s: %w", o.metricsListen, err)
		}
	}

	cfg := gateway.Config{
		RequestTimeout: o.requestTimeout,
		MaxRedirects:   o.maxRedirects,
		MaxReadRetries: o.maxReadRetries,
	}
	metrics := gateway.NewMetrics()
	router, err := gateway.NewStaticRouter(backends, cfg, metrics)
	if err != nil {
		_ = lis.Close()
		if metricsLis != nil {
			_ = metricsLis.Close()
		}
		return err
	}

	fmt.Fprintf(os.Stderr, "peppydb-gateway listening on %s\n", lis.Addr())
	if metricsLis != nil {
		fmt.Fprintf(os.Stderr, "peppydb-gateway metrics listening on %s\n", metricsLis.Addr())
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	return gateway.Serve(ctx, lis, metricsLis, router, cfg, metrics)
}
