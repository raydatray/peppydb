// peppydb-load drives a running peppydb server over gRPC and reports
// throughput and latency (plan 0026, "The load generator").
//
//	peppydb-load --addr 127.0.0.1:7420 --workload mixed:90 --concurrency 64 \
//	    --duration 30s --keys 1000000 --distribution zipf --preload
//
// Workers issue requests back to back (a closed loop). Errors are counted
// by status code and do not stop the run.
package main

import (
	"context"
	"flag"
	"fmt"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"

	"peppydb/lib/load"
	kvv1 "peppydb/proto/kv/v1/go"
)

type options struct {
	addr         string
	workload     string
	concurrency  int
	conns        int
	duration     time.Duration
	warmup       time.Duration
	keys         uint64
	keySize      int
	valueSize    int
	distribution string
	preload      bool
	seed         uint64
	json         bool
}

func main() {
	var o options
	flag.StringVar(&o.addr, "addr", "127.0.0.1:7420", "server address")
	flag.StringVar(&o.workload, "workload", "put", "put, get, or mixed[:read-percent] (default 50)")
	flag.IntVar(&o.concurrency, "concurrency", 64, "concurrent workers")
	flag.IntVar(&o.conns, "conns", 4, "gRPC connections shared by the workers")
	flag.DurationVar(&o.duration, "duration", 10*time.Second, "measured run time")
	flag.DurationVar(&o.warmup, "warmup", 2*time.Second, "unmeasured run time before the measurement")
	flag.Uint64Var(&o.keys, "keys", 100000, "number of distinct keys")
	flag.IntVar(&o.keySize, "key-size", 16, "key size in bytes")
	flag.IntVar(&o.valueSize, "value-size", 100, "value size in bytes")
	flag.StringVar(&o.distribution, "distribution", "uniform", "uniform or zipf[:s]")
	flag.BoolVar(&o.preload, "preload", false, "write every key once before the run")
	flag.Uint64Var(&o.seed, "seed", 1, "random seed")
	flag.BoolVar(&o.json, "json", false, "print JSON instead of text")
	flag.Parse()
	if err := run(o); err != nil {
		fmt.Fprintln(os.Stderr, "peppydb-load:", err)
		os.Exit(1)
	}
}

// readPercent parses the workload: the share of operations that are gets.
func readPercent(workload string) (int, error) {
	name, arg, hasArg := strings.Cut(workload, ":")
	switch {
	case name == "put" && !hasArg:
		return 0, nil
	case name == "get" && !hasArg:
		return 100, nil
	case name == "mixed" && !hasArg:
		return 50, nil
	case name == "mixed":
		p, err := strconv.Atoi(arg)
		if err != nil || p < 0 || p > 100 {
			return 0, fmt.Errorf("mixed read percent must be 0..100: %q", arg)
		}
		return p, nil
	}
	return 0, fmt.Errorf("unknown workload %q", workload)
}

func run(o options) error {
	reads, err := readPercent(o.workload)
	if err != nil {
		return err
	}
	if o.concurrency < 1 || o.conns < 1 || o.keySize < 1 || o.valueSize < 0 {
		return fmt.Errorf("concurrency, conns, and key-size must be positive")
	}
	if _, err := load.NewDistribution(o.distribution, o.keys, o.seed); err != nil {
		return err
	}
	clients := make([]kvv1.KvServiceClient, o.conns)
	for i := range clients {
		conn, err := grpc.NewClient(o.addr, grpc.WithTransportCredentials(insecure.NewCredentials()))
		if err != nil {
			return err
		}
		defer conn.Close()
		clients[i] = kvv1.NewKvServiceClient(conn)
	}
	if o.preload {
		if err := preload(o, clients); err != nil {
			return err
		}
	}

	type workerResult struct{ put, get load.OpResult }
	results := make([]workerResult, o.concurrency)
	start := time.Now()
	measureFrom := start.Add(o.warmup)
	end := measureFrom.Add(o.duration)
	var wg sync.WaitGroup
	for w := range o.concurrency {
		wg.Go(func() {
			r := &results[w]
			r.put.Errors, r.get.Errors = map[string]uint64{}, map[string]uint64{}
			client := clients[w%len(clients)]
			keys, _ := load.NewDistribution(o.distribution, o.keys, o.seed+uint64(w)+1)
			mix := rand.New(rand.NewPCG(o.seed, uint64(w)))
			for {
				began := time.Now()
				if !began.Before(end) {
					return
				}
				i := keys.Next()
				key := load.Key(i, o.keySize)
				op := &r.put
				var err error
				if mix.IntN(100) < reads {
					op = &r.get
					_, err = client.Get(context.Background(), &kvv1.GetRequest{Key: key})
				} else {
					_, err = client.Put(context.Background(), &kvv1.PutRequest{Key: key, Value: load.Value(i, o.valueSize)})
				}
				if began.Before(measureFrom) {
					continue
				}
				if err != nil {
					op.Errors[status.Code(err).String()]++
				} else {
					op.Hist.Record(time.Since(began))
				}
			}
		})
	}
	wg.Wait()

	var put, get load.OpResult
	put.Op, get.Op = "put", "get"
	put.Errors, get.Errors = map[string]uint64{}, map[string]uint64{}
	for _, r := range results {
		for _, pair := range [][2]*load.OpResult{{&put, &r.put}, {&get, &r.get}} {
			pair[0].Hist.Merge(&pair[1].Hist)
			for code, n := range pair[1].Errors {
				pair[0].Errors[code] += n
			}
		}
	}
	put.Duration, get.Duration = o.duration, o.duration
	var reports []load.Report
	if reads < 100 {
		reports = append(reports, put.Report())
	}
	if reads > 0 {
		reports = append(reports, get.Report())
	}
	if o.json {
		return load.WriteJSON(os.Stdout, reports)
	}
	fmt.Printf("addr=%s workload=%s concurrency=%d conns=%d keys=%d distribution=%s key=%dB value=%dB duration=%s\n",
		o.addr, o.workload, o.concurrency, o.conns, o.keys, o.distribution, o.keySize, o.valueSize, o.duration)
	return load.WriteText(os.Stdout, reports)
}

// preload writes every key once, with the workers in parallel.
func preload(o options, clients []kvv1.KvServiceClient) error {
	var wg sync.WaitGroup
	errs := make(chan error, o.concurrency)
	for w := range o.concurrency {
		wg.Go(func() {
			client := clients[w%len(clients)]
			for i := uint64(w); i < o.keys; i += uint64(o.concurrency) {
				req := &kvv1.PutRequest{Key: load.Key(i, o.keySize), Value: load.Value(i, o.valueSize)}
				if _, err := client.Put(context.Background(), req); err != nil {
					errs <- fmt.Errorf("preload key %d: %w", i, err)
					return
				}
			}
		})
	}
	wg.Wait()
	close(errs)
	return <-errs
}
