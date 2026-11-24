package lincheck

import (
	"context"
	"fmt"
	"math/rand/v2"
	"sync"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	kvv1 "peppydb/proto/kv/v1/go"
)

// WorkloadConfig configures a concurrent single-key linearizability run.
type WorkloadConfig struct {
	// Workers is the number of concurrent client goroutines. Defaults to 8.
	Workers int
	// Keys is the number of distinct hot keys (`k0`..`kN-1`). Defaults to 5.
	Keys int
	// Duration bounds how long workers issue requests. Defaults to 3s.
	Duration time.Duration
	// ReadPercent is the share of operations that are Get (0..100). Defaults to 40.
	ReadPercent int
	// DeletePercent is the share of operations that are Delete (0..100). Defaults to 15.
	DeletePercent int
	// OpTimeout bounds each individual RPC. Defaults to 2s.
	OpTimeout time.Duration
	// Seed seeds per-worker PRNGs deterministically. Defaults to 1.
	Seed uint64
	// Fault is an optional callback executed concurrently with the workload
	// (for example, killing and restarting the Raft leader mid-run).
	Fault func(ctx context.Context) error
}

func (c WorkloadConfig) withDefaults() WorkloadConfig {
	if c.Workers <= 0 {
		c.Workers = 8
	}
	if c.Keys <= 0 {
		c.Keys = 5
	}
	if c.Duration <= 0 {
		c.Duration = 3 * time.Second
	}
	if c.ReadPercent == 0 && c.DeletePercent == 0 {
		c.ReadPercent = 40
		c.DeletePercent = 15
	}
	if c.OpTimeout <= 0 {
		c.OpTimeout = 2 * time.Second
	}
	if c.Seed == 0 {
		c.Seed = 1
	}
	return c
}

// RunWorkload drives concurrent Get, Put, and Delete operations against client
// and returns the recorded history. If cfg.Fault is set, it runs concurrently
// with the workers and its error (if any) is returned once all workers finish.
func RunWorkload(ctx context.Context, client kvv1.KvServiceClient, cfg WorkloadConfig) (*Recorder, error) {
	cfg = cfg.withDefaults()
	rec := NewRecorder()

	runCtx, cancel := context.WithTimeout(ctx, cfg.Duration)
	defer cancel()

	var faultErr error
	var faultWg sync.WaitGroup
	if cfg.Fault != nil {
		faultWg.Go(func() {
			faultErr = cfg.Fault(runCtx)
		})
	}

	var wg sync.WaitGroup
	for w := range cfg.Workers {
		wg.Go(func() {
			rng := rand.New(rand.NewPCG(cfg.Seed, uint64(w)+1))
			var seq uint64
			for runCtx.Err() == nil {
				key := fmt.Sprintf("k%d", rng.IntN(cfg.Keys))
				roll := rng.IntN(100)
				opCtx, opCancel := context.WithTimeout(ctx, cfg.OpTimeout)
				call := rec.Now()

				switch {
				case roll < cfg.ReadPercent:
					resp, err := client.Get(opCtx, &kvv1.GetRequest{Key: []byte(key)})
					ret := rec.Now()
					if err == nil {
						rec.RecordGet(w, key, call, ret, resp.Value != nil, string(resp.Value))
					} else {
						rec.RecordFailedGet()
					}
				case roll < cfg.ReadPercent+cfg.DeletePercent:
					_, err := client.Delete(opCtx, &kvv1.DeleteRequest{Key: []byte(key)})
					ret := rec.Now()
					if err == nil {
						rec.RecordDelete(w, key, call, ret, false)
					} else if status.Code(err) != codes.InvalidArgument {
						rec.RecordDelete(w, key, call, ret, true)
					}
				default:
					seq++
					val := fmt.Sprintf("w%d-%d", w, seq)
					_, err := client.Put(opCtx, &kvv1.PutRequest{
						Key:   []byte(key),
						Value: []byte(val),
					})
					ret := rec.Now()
					if err == nil {
						rec.RecordPut(w, key, val, call, ret, false)
					} else if status.Code(err) != codes.InvalidArgument {
						rec.RecordPut(w, key, val, call, ret, true)
					}
				}
				opCancel()
			}
		})
	}

	wg.Wait()
	faultWg.Wait()
	return rec, faultErr
}
